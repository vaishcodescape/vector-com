mod app;
mod net;
mod ui;

use std::io;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures::StreamExt;
use ratatui::{backend::CrosstermBackend, Terminal};
use shared::{ClientMessage, ServerMessage, DEFAULT_PORT};
use tokio::sync::mpsc;

use crate::app::{App, ChatLine, LineKind};

#[derive(Parser, Debug)]
#[command(name = "vectorcom", about = "Vectorcom chat client")]
struct Args {
    #[arg(short = 'H', long, default_value = "127.0.0.1")]
    host: String,
    #[arg(short, long, default_value_t = DEFAULT_PORT)]
    port: u16,
    #[arg(short, long)]
    username: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let username = match args.username {
        Some(u) => u,
        None => prompt_username()?,
    };
    shared::validate_username(&username).map_err(|e| anyhow::anyhow!(e))?;

    let addr = format!("{}:{}", args.host, args.port);
    let (inbound_tx, inbound_rx) = mpsc::channel::<ServerMessage>(128);
    let (outbound_tx, outbound_rx) = mpsc::channel::<ClientMessage>(128);
    let (conn_status_tx, conn_status_rx) = mpsc::channel::<net::ConnStatus>(8);

    let addr_clone = addr.clone();
    let user_clone = username.clone();
    tokio::spawn(async move {
        net::run(addr_clone, user_clone, inbound_tx, outbound_rx, conn_status_tx).await;
    });

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("init terminal")?;

    let mut app = App::new(username.clone(), addr.clone());
    let res = run_app(&mut terminal, &mut app, inbound_rx, outbound_tx, conn_status_rx).await;

    disable_raw_mode().ok();
    execute!(terminal.backend_mut(), LeaveAlternateScreen).ok();
    terminal.show_cursor().ok();
    res
}

fn prompt_username() -> Result<String> {
    use std::io::{BufRead, Write};
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    print!("username: ");
    stdout.flush().ok();
    let mut line = String::new();
    stdin.lock().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

async fn run_app<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    mut inbound: mpsc::Receiver<ServerMessage>,
    outbound: mpsc::Sender<ClientMessage>,
    mut conn: mpsc::Receiver<net::ConnStatus>,
) -> Result<()> {
    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(200));

    loop {
        terminal.draw(|f| ui::draw(f, app))?;

        tokio::select! {
            _ = tick.tick() => {}
            Some(msg) = inbound.recv() => {
                handle_server_msg(app, msg);
            }
            Some(status) = conn.recv() => {
                app.connection = status;
            }
            Some(Ok(ev)) = events.next() => {
                match ev {
                    Event::Key(k) if k.kind != KeyEventKind::Release => {
                        if handle_key(app, k, &outbound).await {
                            return Ok(());
                        }
                    }
                    Event::Resize(_, _) => {}
                    _ => {}
                }
            }
        }
    }
}

fn handle_server_msg(app: &mut App, msg: ServerMessage) {
    match msg {
        ServerMessage::Welcome { room, motd, .. } => {
            app.current_room = room.clone();
            app.push(ChatLine::system(format!("connected · {}", motd)));
            app.push(ChatLine::system(format!("joined #{}", room)));
        }
        ServerMessage::Chat { from, text, ts, .. } => {
            app.push(ChatLine { kind: LineKind::Chat { from }, text, ts });
        }
        ServerMessage::Pm { from, text, ts } => {
            app.push(ChatLine { kind: LineKind::Pm { from, outgoing: false }, text, ts });
        }
        ServerMessage::System { text, ts } => {
            app.push(ChatLine { kind: LineKind::System, text, ts });
        }
        ServerMessage::Error { text } => app.push(ChatLine::error(text)),
        ServerMessage::UserList { users } => {
            app.online = users.clone();
            app.push(ChatLine::system(format!("users in #{}: {}", app.current_room, users.join(", "))));
        }
        ServerMessage::RoomList { rooms } => {
            app.push(ChatLine::system(format!("rooms: {}", rooms.join(", "))));
        }
        ServerMessage::Joined { room } => {
            app.current_room = room.clone();
            app.push(ChatLine::system(format!("→ #{}", room)));
        }
        ServerMessage::UserJoined { username, room } => {
            if room == app.current_room {
                app.push(ChatLine::system(format!("{} joined", username)));
            }
        }
        ServerMessage::UserLeft { username, room } => {
            if room == app.current_room {
                app.push(ChatLine::system(format!("{} left", username)));
            }
        }
    }
}

async fn handle_key(app: &mut App, k: KeyEvent, out: &mpsc::Sender<ClientMessage>) -> bool {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let dropdown = !app.suggestions().is_empty();
    match k.code {
        KeyCode::Esc => {
            if app.input.buf.is_empty() {
                return true;
            }
            app.input.take();
            app.suggest_idx = 0;
        }
        KeyCode::Char('c') if ctrl => return true,
        KeyCode::Char('l') if ctrl => app.history.clear(),
        KeyCode::Tab if dropdown => app.complete_suggestion(),
        KeyCode::Up if dropdown => app.suggest_up(),
        KeyCode::Down if dropdown => app.suggest_down(),
        KeyCode::Enter => {
            let text = app.input.take();
            if !text.is_empty() {
                if let Some(msg) = parse_command(app, &text) {
                    let _ = out.send(msg).await;
                } else if !text.starts_with('/') {
                    let _ = out.send(ClientMessage::Say { text: text.clone() }).await;
                    app.push(ChatLine {
                        kind: LineKind::Chat { from: app.username.clone() },
                        text,
                        ts: shared::now_ts(),
                    });
                }
            }
        }
        KeyCode::Backspace => {
            app.input.backspace();
            app.suggest_idx = 0;
        }
        KeyCode::Delete => app.input.delete(),
        KeyCode::Left => app.input.left(),
        KeyCode::Right => app.input.right(),
        KeyCode::Home => app.input.home(),
        KeyCode::End => app.input.end(),
        KeyCode::Up => app.scroll_up(),
        KeyCode::Down => app.scroll_down(),
        KeyCode::PageUp => app.page_up(),
        KeyCode::PageDown => app.page_down(),
        KeyCode::Char(c) => {
            app.input.insert(c);
            app.suggest_idx = 0;
        }
        _ => {}
    }
    false
}

fn parse_command(app: &mut App, text: &str) -> Option<ClientMessage> {
    if !text.starts_with('/') {
        return None;
    }
    let mut parts = text[1..].splitn(2, ' ');
    let cmd = parts.next().unwrap_or("");
    let rest = parts.next().unwrap_or("").trim();
    match cmd {
        "help" => {
            app.push(ChatLine::system(
                "/join <room>  /list  /rooms  /pm <user> <msg>  /clear  /quit".into(),
            ));
            None
        }
        "quit" | "exit" => Some(ClientMessage::Quit),
        "list" | "users" => Some(ClientMessage::ListUsers),
        "rooms" => Some(ClientMessage::ListRooms),
        "join" if !rest.is_empty() => Some(ClientMessage::Join { room: rest.into() }),
        "pm" | "msg" | "whisper" => {
            let mut p = rest.splitn(2, ' ');
            match (p.next(), p.next()) {
                (Some(to), Some(msg)) if !to.is_empty() && !msg.is_empty() => {
                    app.push(ChatLine {
                        kind: LineKind::Pm { from: to.into(), outgoing: true },
                        text: msg.into(),
                        ts: shared::now_ts(),
                    });
                    Some(ClientMessage::Pm { to: to.into(), text: msg.into() })
                }
                _ => {
                    app.push(ChatLine::error("usage: /pm <user> <message>".into()));
                    None
                }
            }
        }
        "block" | "unblock" | "blocklist" => {
            app.push(ChatLine::error("blocking is managed by the server admin".into()));
            None
        }
        "clear" => {
            app.history.clear();
            None
        }
        _ => {
            app.push(ChatLine::error(format!("unknown command: /{} (try /help)", cmd)));
            None
        }
    }
}
