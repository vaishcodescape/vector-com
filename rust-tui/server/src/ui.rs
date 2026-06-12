use std::io;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures::StreamExt;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Cell, Clear, List, ListItem, Paragraph, Row, Table};
use ratatui::{backend::CrosstermBackend, Terminal};
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::state::ServerState;

const ACCENT: Color = Color::Rgb(135, 206, 250);
const MUTED: Color = Color::Rgb(140, 140, 140);
const DIM: Color = Color::Rgb(90, 90, 90);

/// Admin commands: (name, usage, help). Typed plain or with a leading '/',
/// which opens the dropdown; the slash is stripped before dispatch.
const ADMIN_COMMANDS: &[(&str, &str, &str)] = &[
    ("kick", "kick <user>", "disconnect a user"),
    ("say", "say <message>", "broadcast to all rooms"),
    ("block", "block <user>", "block a user from sending messages"),
    ("unblock", "unblock <user>", "unblock a user"),
    ("slowmode", "slowmode <room> <secs>", "set per-room message cooldown"),
];

fn suggestions(input: &str) -> Vec<&'static (&'static str, &'static str, &'static str)> {
    if input.is_empty() || input.contains(' ') {
        return Vec::new();
    }
    let typed = input.strip_prefix('/').unwrap_or(input);
    ADMIN_COMMANDS.iter().filter(|(name, _, _)| name.starts_with(typed)).collect()
}

pub async fn run(state: Arc<ServerState>, addr: String, admin_tx: mpsc::Sender<String>) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let res = event_loop(&mut terminal, state, addr, admin_tx).await;

    disable_raw_mode().ok();
    execute!(terminal.backend_mut(), LeaveAlternateScreen).ok();
    terminal.show_cursor().ok();
    res
}

async fn event_loop<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    state: Arc<ServerState>,
    addr: String,
    admin_tx: mpsc::Sender<String>,
) -> Result<()> {
    let mut events = EventStream::new();
    let started = Instant::now();
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    let mut input = String::new();
    let mut suggest_idx: usize = 0;

    loop {
        state.set_uptime(started.elapsed());
        let snap = state.snapshot();
        terminal.draw(|f| draw(f, &snap, &addr, &input, suggest_idx))?;

        tokio::select! {
            _ = tick.tick() => {}
            Some(Ok(ev)) = events.next() => {
                if let Event::Key(k) = ev {
                    if k.kind == KeyEventKind::Release {
                        continue;
                    }
                    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
                    let sugg_count = suggestions(&input).len();
                    match k.code {
                        KeyCode::Char('c') if ctrl => return Ok(()),
                        KeyCode::Esc => {
                            if input.is_empty() {
                                return Ok(());
                            }
                            input.clear();
                            suggest_idx = 0;
                        }
                        KeyCode::Up if sugg_count > 0 => {
                            suggest_idx = suggest_idx.min(sugg_count - 1)
                                .checked_sub(1).unwrap_or(sugg_count - 1);
                        }
                        KeyCode::Down if sugg_count > 0 => {
                            suggest_idx = (suggest_idx.min(sugg_count - 1) + 1) % sugg_count;
                        }
                        KeyCode::Tab if sugg_count > 0 => {
                            let (name, _, _) = suggestions(&input)[suggest_idx.min(sugg_count - 1)];
                            input = format!("{} ", name);
                            suggest_idx = 0;
                        }
                        KeyCode::Enter => {
                            // Commands may be typed with a leading '/'.
                            let cmd = input.trim().trim_start_matches('/').to_string();
                            input.clear();
                            suggest_idx = 0;
                            if !cmd.is_empty() {
                                let _ = admin_tx.send(cmd).await;
                            }
                        }
                        KeyCode::Backspace => {
                            input.pop();
                            suggest_idx = 0;
                        }
                        KeyCode::Char(c) if !ctrl => {
                            input.push(c);
                            suggest_idx = 0;
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}

fn draw(f: &mut ratatui::Frame, snap: &crate::state::Snapshot, addr: &str, input: &str, suggest_idx: usize) {
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(area);

    draw_header(f, chunks[0], snap, addr);

    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(chunks[1]);

    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(body[0]);

    draw_rooms(f, left[0], snap);
    draw_users(f, left[1], snap);
    draw_log(f, body[1], snap);
    draw_input(f, chunks[2], input);
    draw_footer(f, chunks[3]);
    draw_command_dropdown(f, chunks[2], input, suggest_idx);

    // Place the cursor after the typed text inside the input box.
    let cursor_x = chunks[2].x + 4 + input.chars().count() as u16;
    if cursor_x < chunks[2].x + chunks[2].width.saturating_sub(2) {
        f.set_cursor_position((cursor_x, chunks[2].y + 1));
    }
}

fn draw_input(f: &mut ratatui::Frame, area: Rect, input: &str) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(DIM))
        .title(Span::styled(" admin ", Style::default().fg(MUTED)));
    let prompt = Span::styled(" › ", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD));
    let body: Span = if input.is_empty() {
        Span::styled(
            "type / for commands — kick · say · block · unblock · slowmode",
            Style::default().fg(DIM).add_modifier(Modifier::ITALIC),
        )
    } else {
        Span::styled(input.to_string(), Style::default().fg(Color::White))
    };
    f.render_widget(Paragraph::new(Line::from(vec![prompt, body])).block(block), area);
}

/// Dropdown listing matching admin commands with help text, anchored above
/// the admin prompt while the first word is being typed.
fn draw_command_dropdown(f: &mut ratatui::Frame, input_area: Rect, input: &str, suggest_idx: usize) {
    let sugg = suggestions(input);
    if sugg.is_empty() {
        return;
    }
    let selected = suggest_idx.min(sugg.len() - 1);

    let usage_w = sugg.iter().map(|(_, usage, _)| usage.len()).max().unwrap_or(0);
    let height = (sugg.len() as u16 + 2).min(input_area.y);
    if height < 3 {
        return;
    }
    let width = (usage_w + sugg.iter().map(|(_, _, help)| help.len()).max().unwrap_or(0) + 7)
        .min(input_area.width.saturating_sub(4) as usize) as u16;
    let area = Rect {
        x: input_area.x + 2,
        y: input_area.y - height,
        width,
        height,
    };

    let lines: Vec<Line> = sugg
        .iter()
        .enumerate()
        .map(|(i, (_, usage, help))| {
            let (usage_style, help_style) = if i == selected {
                (
                    Style::default().fg(Color::Black).bg(ACCENT).add_modifier(Modifier::BOLD),
                    Style::default().fg(Color::Black).bg(ACCENT),
                )
            } else {
                (Style::default().fg(Color::White), Style::default().fg(MUTED))
            };
            Line::from(vec![
                Span::styled(format!(" {:<w$}  ", usage, w = usage_w), usage_style),
                Span::styled(format!("{} ", help), help_style),
            ])
        })
        .collect();

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(DIM))
        .title(Span::styled(" admin commands ", Style::default().fg(MUTED)))
        .title_bottom(Span::styled(" ↑↓ select · tab complete ", Style::default().fg(DIM)));
    f.render_widget(Clear, area);
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_header(f: &mut ratatui::Frame, area: Rect, snap: &crate::state::Snapshot, addr: &str) {
    let uptime = fmt_uptime(snap.uptime);
    let title = Line::from(vec![
        Span::styled(" ◆ ", Style::default().fg(ACCENT)),
        Span::styled("Vectorcom", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
        Span::styled("  server", Style::default().fg(MUTED)),
        Span::styled("    ", Style::default()),
        Span::styled(addr, Style::default().fg(Color::White)),
    ]);
    let stats = Line::from(vec![
        Span::styled("uptime ", Style::default().fg(DIM)),
        Span::styled(uptime, Style::default().fg(Color::White)),
        Span::styled("   conn ", Style::default().fg(DIM)),
        Span::styled(snap.total_connections.to_string(), Style::default().fg(Color::White)),
        Span::styled("   msg ", Style::default().fg(DIM)),
        Span::styled(snap.total_messages.to_string(), Style::default().fg(Color::White)),
        Span::styled("   online ", Style::default().fg(DIM)),
        Span::styled(snap.users.len().to_string(), Style::default().fg(Color::Green)),
    ]);
    let para = Paragraph::new(vec![title, stats]).block(
        Block::default()
            .borders(Borders::BOTTOM)
            .border_type(BorderType::Plain)
            .border_style(Style::default().fg(DIM)),
    );
    f.render_widget(para, area);
}

fn draw_rooms(f: &mut ratatui::Frame, area: Rect, snap: &crate::state::Snapshot) {
    let items: Vec<ListItem> = snap.rooms.iter().map(|(name, count)| {
        ListItem::new(Line::from(vec![
            Span::styled(format!(" #{:<16}", name), Style::default().fg(Color::White)),
            Span::styled(format!("{} online", count), Style::default().fg(MUTED)),
        ]))
    }).collect();
    let list = List::new(items).block(panel(" rooms "));
    f.render_widget(list, area);
}

fn draw_users(f: &mut ratatui::Frame, area: Rect, snap: &crate::state::Snapshot) {
    let rows: Vec<Row> = snap.users.iter().map(|(u, r)| {
        Row::new(vec![
            Cell::from(Span::styled(format!(" {}", u), Style::default().fg(user_color(u)))),
            Cell::from(Span::styled(format!("#{}", r), Style::default().fg(MUTED))),
        ])
    }).collect();
    let table = Table::new(rows, [Constraint::Percentage(55), Constraint::Percentage(45)])
        .block(panel(" users "));
    f.render_widget(table, area);
}

fn draw_log(f: &mut ratatui::Frame, area: Rect, snap: &crate::state::Snapshot) {
    let take = area.height.saturating_sub(2) as usize;
    let lines: Vec<Line> = snap.log.iter().rev().take(take).rev().map(|l| {
        Line::from(vec![
            Span::styled(format!(" {} ", fmt_time(l.ts)), Style::default().fg(DIM)),
            Span::styled(&l.text, Style::default().fg(Color::White)),
        ])
    }).collect();
    let para = Paragraph::new(lines).block(panel(" activity "));
    f.render_widget(para, area);
}

fn draw_footer(f: &mut ratatui::Frame, area: Rect) {
    let p = Paragraph::new(Line::from(vec![
        Span::styled("  enter ", Style::default().fg(ACCENT)),
        Span::styled("run admin command", Style::default().fg(MUTED)),
        Span::styled("    esc ", Style::default().fg(ACCENT)),
        Span::styled("clear / quit", Style::default().fg(MUTED)),
        Span::styled("    ctrl-c ", Style::default().fg(ACCENT)),
        Span::styled("quit", Style::default().fg(MUTED)),
    ]));
    f.render_widget(p, area);
}

fn panel(title: &str) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(DIM))
        .title(Span::styled(title.to_string(), Style::default().fg(MUTED)))
}

fn fmt_uptime(d: Duration) -> String {
    let s = d.as_secs();
    let (h, m, s) = (s / 3600, (s % 3600) / 60, s % 60);
    format!("{:02}:{:02}:{:02}", h, m, s)
}

fn fmt_time(ts: i64) -> String {
    use chrono::{DateTime, Local, TimeZone};
    let dt: DateTime<Local> = Local.timestamp_opt(ts, 0).single().unwrap_or_else(|| Local.timestamp_opt(0, 0).unwrap());
    dt.format("%H:%M:%S").to_string()
}

pub fn user_color(name: &str) -> Color {
    const PALETTE: &[Color] = &[
        Color::Rgb(217, 119, 87),
        Color::Rgb(120, 180, 220),
        Color::Rgb(180, 200, 120),
        Color::Rgb(200, 140, 200),
        Color::Rgb(220, 180, 120),
        Color::Rgb(140, 200, 180),
    ];
    let mut h: u32 = 5381;
    for b in name.as_bytes() {
        h = h.wrapping_mul(33).wrapping_add(*b as u32);
    }
    PALETTE[(h as usize) % PALETTE.len()]
}
