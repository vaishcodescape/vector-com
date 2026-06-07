use std::time::Duration;

use shared::{xor_bytes, ClientMessage, ServerMessage};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub enum ConnStatus {
    Connecting,
    Online,
    Reconnecting,
    Closed,
}

pub async fn run(
    addr: String,
    username: String,
    inbound: mpsc::Sender<ServerMessage>,
    mut outbound: mpsc::Receiver<ClientMessage>,
    status: mpsc::Sender<ConnStatus>,
) {
    let mut backoff = Duration::from_millis(500);
    let mut first_connect = true;
    loop {
        let _ = status.send(ConnStatus::Connecting).await;
        let stream = match TcpStream::connect(&addr).await {
            Ok(s) => s,
            Err(e) => {
                let _ = status.send(ConnStatus::Reconnecting).await;
                let _ = inbound.send(ServerMessage::Error { text: format!("connect: {}", e) }).await;
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(10));
                continue;
            }
        };
        stream.set_nodelay(true).ok();
        backoff = Duration::from_millis(500);

        let (mut read_half, mut write_half) = stream.into_split();

        if write_half.write_all(username.as_bytes()).await.is_err() {
            let _ = status.send(ConnStatus::Reconnecting).await;
            let _ = inbound.send(ServerMessage::Error { text: "username send failed, retrying...".into() }).await;
            continue;
        }

        let _ = status.send(ConnStatus::Online).await;
        if first_connect {
            let _ = inbound
                .send(ServerMessage::Welcome {
                    username: username.clone(),
                    room: "general".into(),
                    motd: format!("connected to {} (xor-encrypted)", addr),
                })
                .await;
            first_connect = false;
        } else {
            let _ = inbound
                .send(ServerMessage::System {
                    text: "reconnected".into(),
                    ts: shared::now_ts(),
                })
                .await;
        }

        let read_inbound = inbound.clone();
        let read_task = tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let mut collector = ListCollector::default();
            loop {
                match read_half.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let decrypted = xor_bytes(&buf[..n]);
                        let text = String::from_utf8_lossy(&decrypted).to_string();
                        for raw in text.split('\n') {
                            let line = raw.trim_end_matches('\r');
                            if line.is_empty() {
                                continue;
                            }
                            for msg in collector.feed(line) {
                                if read_inbound.send(msg).await.is_err() {
                                    return;
                                }
                            }
                        }
                        // A `/list` or `/rooms` reply arrives as one block per
                        // recv; flush it so the structured list is emitted now.
                        for msg in collector.flush() {
                            if read_inbound.send(msg).await.is_err() {
                                return;
                            }
                        }
                    }
                }
            }
        });

        loop {
            tokio::select! {
                msg = outbound.recv() => {
                    let Some(msg) = msg else { return; };
                    let quit = matches!(msg, ClientMessage::Quit);
                    let Some(wire) = render_outbound(&msg) else { continue; };
                    let encrypted = xor_bytes(wire.as_bytes());
                    if write_half.write_all(&encrypted).await.is_err() {
                        break;
                    }
                    if quit {
                        let _ = status.send(ConnStatus::Closed).await;
                        read_task.abort();
                        return;
                    }
                }
                _ = wait_done(&read_task) => {
                    break;
                }
            }
        }

        read_task.abort();
        let _ = status.send(ConnStatus::Reconnecting).await;
        let _ = inbound.send(ServerMessage::Error { text: "lost connection, retrying...".into() }).await;
        tokio::time::sleep(Duration::from_millis(800)).await;
    }
}

fn render_outbound(msg: &ClientMessage) -> Option<String> {
    match msg {
        ClientMessage::Hello { .. } => None,
        ClientMessage::Say { text } => Some(text.clone()),
        ClientMessage::Join { room } => Some(format!("/join {}", room)),
        ClientMessage::ListUsers => Some("/list".into()),
        ClientMessage::ListRooms => Some("/rooms".into()),
        ClientMessage::Pm { to, text } => Some(format!("/pm {} {}", to, text)),
        ClientMessage::Block { username } => Some(format!("/block {}", username)),
        ClientMessage::Unblock { username } => Some(format!("/unblock {}", username)),
        ClientMessage::Quit => Some("/quit".into()),
    }
}

fn parse_line(line: &str) -> ServerMessage {
    let ts = shared::now_ts();

    // PM:  "[PM from X] text"
    if let Some(rest) = line.strip_prefix("[PM from ") {
        if let Some(end) = rest.find(']') {
            let from = rest[..end].to_string();
            let text = rest[end + 1..].trim_start().to_string();
            return ServerMessage::Pm { from, text, ts };
        }
    }

    // "[HH:MM:SS] ..." — chat, join, or leave
    if let Some(rest) = strip_bracketed_timestamp(line) {
        // join: "<user> joined the chat (room: <r>)" or "<user> joined room <r>"
        if let Some((user, tail)) = split_first_word(rest) {
            if tail.starts_with("joined ") {
                let room = if let Some(r) = tail.strip_prefix("joined room ") {
                    r.trim().to_string()
                } else if let Some(r) = tail.strip_prefix("joined the chat (room: ") {
                    r.trim_end_matches(')').to_string()
                } else {
                    String::new()
                };
                return ServerMessage::UserJoined { username: user.to_string(), room };
            }
            if tail.starts_with("left ") {
                let room = tail.strip_prefix("left room ").unwrap_or("").trim().to_string();
                return ServerMessage::UserLeft { username: user.to_string(), room };
            }
        }
        // chat: "<user>: <text>"
        if let Some(colon) = rest.find(": ") {
            let from = rest[..colon].to_string();
            let text = rest[colon + 2..].to_string();
            if !from.is_empty() && !from.contains(' ') {
                return ServerMessage::Chat { from, room: String::new(), text, ts };
            }
        }
    }

    ServerMessage::System { text: line.to_string(), ts }
}

fn strip_bracketed_timestamp(line: &str) -> Option<&str> {
    let rest = line.strip_prefix('[')?;
    let close = rest.find(']')?;
    let ts = &rest[..close];
    if ts.len() == 8 && ts.as_bytes()[2] == b':' && ts.as_bytes()[5] == b':' {
        Some(rest[close + 1..].trim_start())
    } else {
        None
    }
}

fn split_first_word(s: &str) -> Option<(&str, &str)> {
    let sp = s.find(' ')?;
    Some((&s[..sp], &s[sp + 1..]))
}

#[derive(Default, PartialEq)]
enum CollectMode {
    #[default]
    None,
    Users,
    Rooms,
}

/// Reassembles the multi-line `/list` and `/rooms` text replies back into
/// structured `UserList` / `RoomList` messages. Lines outside those blocks pass
/// straight through `parse_line`.
#[derive(Default)]
struct ListCollector {
    mode: CollectMode,
    items: Vec<String>,
}

impl ListCollector {
    fn feed(&mut self, line: &str) -> Vec<ServerMessage> {
        let trimmed = line.trim();
        if trimmed == shared::USER_LIST_HEADER {
            let out = self.flush();
            self.mode = CollectMode::Users;
            return out;
        }
        if trimmed == shared::ROOM_LIST_HEADER {
            let out = self.flush();
            self.mode = CollectMode::Rooms;
            return out;
        }
        if self.mode != CollectMode::None {
            if let Some(item) = shared::list_item(line) {
                self.items.push(item.to_string());
                return Vec::new();
            }
            // Non-list line ends the block: emit the list, then this line.
            let mut out = self.flush();
            out.push(parse_line(line));
            return out;
        }
        vec![parse_line(line)]
    }

    fn flush(&mut self) -> Vec<ServerMessage> {
        let items = std::mem::take(&mut self.items);
        match std::mem::take(&mut self.mode) {
            CollectMode::Users => {
                let users = items.iter().map(|i| shared::parse_user_entry(i).0).collect();
                vec![ServerMessage::UserList { users }]
            }
            CollectMode::Rooms => {
                let rooms = items.iter().map(|i| shared::parse_room_entry(i).0).collect();
                vec![ServerMessage::RoomList { rooms }]
            }
            CollectMode::None => Vec::new(),
        }
    }
}

async fn wait_done(handle: &tokio::task::JoinHandle<()>) {
    while !handle.is_finished() {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
