use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use crate::app::{App, ChatLine, LineKind};
use crate::net::ConnStatus;

pub const ACCENT: Color = Color::Rgb(135, 206, 250);
pub const MUTED: Color = Color::Rgb(150, 150, 150);
pub const DIM: Color = Color::Rgb(95, 95, 95);
pub const SOFT: Color = Color::Rgb(210, 210, 210);

pub fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(3),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(area);

    draw_header(f, chunks[0], app);
    draw_history(f, chunks[1], app);
    draw_input(f, chunks[2], app);
    draw_hints(f, chunks[3]);

    let input_inner_x = chunks[2].x + 4 + visible_input_width(&app.input.buf[..app.input.cursor]) as u16;
    let input_inner_y = chunks[2].y + 1;
    if input_inner_x < chunks[2].x + chunks[2].width.saturating_sub(2) {
        f.set_cursor_position((input_inner_x, input_inner_y));
    }
}

fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    let (dot, status_label, status_color) = match &app.connection {
        ConnStatus::Online => ("●", "online".to_string(), Color::Green),
        ConnStatus::Connecting => ("◐", "connecting".to_string(), Color::Yellow),
        ConnStatus::Reconnecting => ("◑", "reconnecting".to_string(), Color::Yellow),
        ConnStatus::Closed => ("○", "closed".to_string(), Color::Red),
    };

    let left = Line::from(vec![
        Span::styled(" ◆ ", Style::default().fg(ACCENT)),
        Span::styled("Vectorcom", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
        Span::styled("  ·  ", Style::default().fg(DIM)),
        Span::styled(format!("#{}", app.current_room), Style::default().fg(SOFT).add_modifier(Modifier::BOLD)),
        Span::styled("  ·  ", Style::default().fg(DIM)),
        Span::styled(format!("{} online", app.online.len().max(1)), Style::default().fg(MUTED)),
        Span::styled("  ·  ", Style::default().fg(DIM)),
        Span::styled(&app.username, Style::default().fg(user_color(&app.username))),
    ]);

    let right = Line::from(vec![
        Span::styled(dot, Style::default().fg(status_color)),
        Span::styled(format!(" {}  ", status_label), Style::default().fg(MUTED)),
        Span::styled(&app.server_addr, Style::default().fg(DIM)),
        Span::raw(" "),
    ]).alignment(ratatui::layout::Alignment::Right);

    let split = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(Rect { x: area.x, y: area.y, width: area.width, height: 1 });
    f.render_widget(Paragraph::new(left), split[0]);
    f.render_widget(Paragraph::new(right), split[1]);

    let rule = "─".repeat(area.width as usize);
    f.render_widget(
        Paragraph::new(Span::styled(rule, Style::default().fg(DIM))),
        Rect { x: area.x, y: area.y + 1, width: area.width, height: 1 },
    );
}

fn draw_history(f: &mut Frame, area: Rect, app: &App) {
    let width = area.width.saturating_sub(2) as usize;
    let mut lines: Vec<Line> = Vec::new();
    for line in app.history.iter() {
        for l in render_chat_line(line, width, &app.username) {
            lines.push(l);
        }
    }

    let total = lines.len();
    let visible = area.height as usize;
    let offset = app.scroll_offset.min(total.saturating_sub(visible));
    let start = total.saturating_sub(visible).saturating_sub(offset);
    let end = (start + visible).min(total);
    let slice: Vec<Line> = lines[start..end].to_vec();

    let para = Paragraph::new(slice).wrap(Wrap { trim: false });
    f.render_widget(para, area);

    if app.scroll_offset > 0 {
        let badge = format!(" ↑ {} ", app.scroll_offset);
        let bw = badge.width() as u16;
        if area.width > bw + 2 {
            let r = Rect { x: area.x + area.width - bw - 1, y: area.y, width: bw, height: 1 };
            f.render_widget(
                Paragraph::new(Span::styled(badge, Style::default().fg(Color::Black).bg(ACCENT))),
                r,
            );
        }
    }
}

fn render_chat_line(line: &ChatLine, _width: usize, self_name: &str) -> Vec<Line<'static>> {
    let ts = fmt_time(line.ts);
    match &line.kind {
        LineKind::Chat { from } => {
            let is_self = from == self_name;
            let name_style = if is_self {
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(user_color(from)).add_modifier(Modifier::BOLD)
            };
            vec![Line::from(vec![
                Span::styled(format!(" {} ", ts), Style::default().fg(DIM)),
                Span::styled(format!("{:<10}", truncate(from, 10)), name_style),
                Span::styled("  ", Style::default()),
                Span::styled(line.text.clone(), Style::default().fg(SOFT)),
            ])]
        }
        LineKind::Pm { from, outgoing } => {
            let label = if *outgoing { format!("→ {}", from) } else { format!("← {}", from) };
            vec![Line::from(vec![
                Span::styled(format!(" {} ", ts), Style::default().fg(DIM)),
                Span::styled(format!("{:<10}", truncate(&label, 10)), Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD)),
                Span::styled("  ", Style::default()),
                Span::styled(line.text.clone(), Style::default().fg(Color::Rgb(220, 180, 230))),
            ])]
        }
        LineKind::System => vec![Line::from(vec![
            Span::styled(format!(" {} ", ts), Style::default().fg(DIM)),
            Span::styled(format!("{:<10}", "·"), Style::default().fg(DIM)),
            Span::styled("  ", Style::default()),
            Span::styled(line.text.clone(), Style::default().fg(MUTED).add_modifier(Modifier::ITALIC)),
        ])],
        LineKind::Error => vec![Line::from(vec![
            Span::styled(format!(" {} ", ts), Style::default().fg(DIM)),
            Span::styled(format!("{:<10}", "!"), Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
            Span::styled("  ", Style::default()),
            Span::styled(line.text.clone(), Style::default().fg(Color::Rgb(240, 130, 120))),
        ])],
    }
}

fn draw_input(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(DIM));
    let placeholder_style = Style::default().fg(DIM).add_modifier(Modifier::ITALIC);
    let prompt = Span::styled(" › ", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD));
    let body: Span = if app.input.buf.is_empty() {
        Span::styled("Type a message or /help for commands", placeholder_style)
    } else {
        Span::styled(app.input.buf.clone(), Style::default().fg(Color::White))
    };
    let para = Paragraph::new(Line::from(vec![prompt, body])).block(block);
    f.render_widget(para, area);
}

fn draw_hints(f: &mut Frame, area: Rect) {
    let p = Paragraph::new(Line::from(vec![
        Span::styled("  enter ", Style::default().fg(ACCENT)),
        Span::styled("send", Style::default().fg(MUTED)),
        Span::styled("   / ", Style::default().fg(ACCENT)),
        Span::styled("commands", Style::default().fg(MUTED)),
        Span::styled("   ↑↓ ", Style::default().fg(ACCENT)),
        Span::styled("scroll", Style::default().fg(MUTED)),
        Span::styled("   ctrl-l ", Style::default().fg(ACCENT)),
        Span::styled("clear", Style::default().fg(MUTED)),
        Span::styled("   esc ", Style::default().fg(ACCENT)),
        Span::styled("quit", Style::default().fg(MUTED)),
    ]));
    f.render_widget(p, area);
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

fn visible_input_width(s: &str) -> usize {
    s.width()
}

fn fmt_time(ts: i64) -> String {
    use chrono::{DateTime, Local, TimeZone};
    let dt: DateTime<Local> = Local.timestamp_opt(ts, 0).single().unwrap_or_else(|| Local.timestamp_opt(0, 0).unwrap());
    dt.format("%H:%M").to_string()
}

pub fn user_color(name: &str) -> Color {
    const PALETTE: &[Color] = &[
        Color::Rgb(217, 119, 87),
        Color::Rgb(120, 180, 220),
        Color::Rgb(180, 200, 120),
        Color::Rgb(200, 140, 200),
        Color::Rgb(220, 180, 120),
        Color::Rgb(140, 200, 180),
        Color::Rgb(230, 150, 150),
    ];
    let mut h: u32 = 5381;
    for b in name.as_bytes() {
        h = h.wrapping_mul(33).wrapping_add(*b as u32);
    }
    PALETTE[(h as usize) % PALETTE.len()]
}
