mod state;
mod ui;
mod wire;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use shared::{xor_bytes, ClientMessage, ServerMessage, DEFAULT_PORT, DEFAULT_ROOM, MAX_MESSAGE_BYTES};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

use crate::state::{ClientId, ServerState};

#[derive(Parser, Debug)]
#[command(name = "vectorcom-server", about = "Vectorcom chat server (C++ wire-compatible)")]
struct Args {
    #[arg(short, long, default_value_t = DEFAULT_PORT)]
    port: u16,
    #[arg(long, default_value = "0.0.0.0")]
    bind: String,
    #[arg(long, help = "Run without TUI (plain log output)")]
    headless: bool,
    /// Run as a read-only observer attached to an upstream vector-com server.
    #[arg(long, value_name = "HOST:PORT")]
    observe: Option<String>,
    /// Username to use when observing (default: "observer").
    #[arg(long, default_value = "observer")]
    observe_as: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let state = Arc::new(ServerState::new());

    if let Some(upstream) = args.observe.clone() {
        state.set_observe_mode(true);
        let (admin_tx, admin_rx) = mpsc::channel::<String>(32);
        let st = state.clone();
        let name = args.observe_as.clone();
        let up = upstream.clone();
        tokio::spawn(async move {
            observe_loop(st, up, name, admin_rx).await;
        });
        let title = format!("observing {}", upstream);
        if args.headless {
            state.log(title);
            futures::future::pending::<()>().await;
        } else {
            ui::run(state.clone(), title, admin_tx).await?;
        }
        return Ok(());
    }

    let addr = format!("{}:{}", args.bind, args.port);
    let listener = TcpListener::bind(&addr).await?;

    let state_clone = state.clone();
    let addr_clone = addr.clone();
    let accept_task = tokio::spawn(async move {
        accept_loop(listener, state_clone, addr_clone).await;
    });

    if args.headless {
        state.log(format!("listening on {}", addr));
        accept_task.await.ok();
    } else {
        // Standalone server: admin commands typed in the dashboard act directly
        // on local state.
        let (admin_tx, mut admin_rx) = mpsc::channel::<String>(32);
        let st = state.clone();
        tokio::spawn(async move {
            while let Some(cmd) = admin_rx.recv().await {
                apply_admin_local(&st, &cmd);
            }
        });
        ui::run(state.clone(), addr, admin_tx).await?;
        accept_task.abort();
    }
    Ok(())
}

/// Apply an admin command against local state (standalone Rust server).
/// `block`/`unblock` are special commands; anything else (including an
/// optional `say ` prefix) is broadcast to all clients as-is.
fn apply_admin_local(state: &Arc<ServerState>, cmd: &str) {
    let cmd = cmd.trim();
    if let Some(rest) = cmd.strip_prefix("say ") {
        state.admin_say(rest.to_string());
        state.log(format!("[admin] say: {}", rest));
    } else if let Some(user) = cmd.strip_prefix("block ") {
        state.admin_block(user.trim(), true);
        state.log(format!("[admin] blocked: {}", user.trim()));
    } else if let Some(user) = cmd.strip_prefix("unblock ") {
        state.admin_block(user.trim(), false);
        state.log(format!("[admin] unblocked: {}", user.trim()));
    } else if !cmd.is_empty() {
        // Anything else typed by the admin is broadcast as-is.
        state.admin_say(cmd.to_string());
        state.log(format!("[admin] say: {}", cmd));
    }
}

async fn accept_loop(listener: TcpListener, state: Arc<ServerState>, addr: String) {
    state.log(format!("listening on {} (xor-encrypted)", addr));
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                let st = state.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_client(stream, st.clone(), peer.to_string()).await {
                        st.log(format!("client {} dropped: {}", peer, e));
                    }
                });
            }
            Err(e) => state.log(format!("accept error: {}", e)),
        }
    }
}

async fn handle_client(stream: TcpStream, state: Arc<ServerState>, peer: String) -> Result<()> {
    stream.set_nodelay(true).ok();
    let (mut read_half, mut write_half) = stream.into_split();

    let mut name_buf = [0u8; 128];
    let n = tokio::time::timeout(Duration::from_secs(15), read_half.read(&mut name_buf))
        .await
        .map_err(|_| anyhow::anyhow!("handshake timeout"))??;
    if n == 0 {
        anyhow::bail!("empty handshake");
    }
    let mut username = String::from_utf8_lossy(&name_buf[..n]).to_string();
    while matches!(username.chars().last(), Some('\n') | Some('\r')) {
        username.pop();
    }
    if username.is_empty() {
        username = "Anonymous".into();
    }
    if username.len() > 63 {
        username.truncate(63);
    }
    if let Err(e) = shared::validate_username(&username) {
        let err = wire::render_server(&ServerMessage::Error { text: e.into() });
        let _ = write_half.write_all(&xor_bytes(err.as_bytes())).await;
        anyhow::bail!("bad username: {}", e);
    }

    let (tx, mut rx) = mpsc::channel::<ServerMessage>(64);
    let id = match state.register(username.clone(), tx.clone()) {
        Ok(id) => id,
        Err(e) => {
            let err = wire::render_server(&ServerMessage::Error { text: e.into() });
            let _ = write_half.write_all(&xor_bytes(err.as_bytes())).await;
            anyhow::bail!("register failed: {}", e);
        }
    };
    state.log(format!("+ {} ({}) joined from {}", username, id.0, peer));

    let _ = tx
        .send(ServerMessage::Welcome {
            username: username.clone(),
            room: DEFAULT_ROOM.into(),
            motd: "Welcome to Vectorcom. Type /help for commands.".into(),
        })
        .await;
    state.broadcast_room(
        DEFAULT_ROOM,
        ServerMessage::UserJoined { username: username.clone(), room: DEFAULT_ROOM.into() },
        Some(id),
    );

    let writer_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            let text = wire::render_server(&msg);
            if write_half.write_all(&xor_bytes(text.as_bytes())).await.is_err() {
                break;
            }
        }
    });

    let read_result = read_loop(&mut read_half, state.clone(), id).await;

    state.unregister(id);
    state.log(format!("- {} ({}) left", username, id.0));
    writer_task.abort();
    read_result
}

async fn read_loop(
    read_half: &mut tokio::net::tcp::OwnedReadHalf,
    state: Arc<ServerState>,
    id: ClientId,
) -> Result<()> {
    let mut buf = [0u8; 2048];
    loop {
        let n = read_half.read(&mut buf).await?;
        if n == 0 {
            return Ok(());
        }
        if n > MAX_MESSAGE_BYTES {
            state.send_to(id, ServerMessage::Error { text: "message too large".into() });
            continue;
        }
        let decrypted = xor_bytes(&buf[..n]);
        let text = String::from_utf8_lossy(&decrypted).to_string();
        for raw in text.split('\n') {
            let line = raw.trim_end_matches('\r').trim();
            if line.is_empty() {
                continue;
            }
            state.bump_msg_count();
            match wire::parse_client(line) {
                ClientMessage::Hello { .. } => {}
                ClientMessage::Say { text } => state.handle_say(id, text),
                ClientMessage::Join { room } => state.handle_join(id, room),
                ClientMessage::ListUsers => state.handle_list_users(id),
                ClientMessage::ListRooms => state.handle_list_rooms(id),
                ClientMessage::Pm { to, text } => state.handle_pm(id, to, text),
                ClientMessage::Block { username } => state.handle_block(id, username, true),
                ClientMessage::Unblock { username } => state.handle_block(id, username, false),
                ClientMessage::Quit => return Ok(()),
            }
        }
    }
}

async fn observe_loop(
    state: Arc<ServerState>,
    addr: String,
    username: String,
    mut admin_rx: mpsc::Receiver<String>,
) {
    let mut backoff = Duration::from_millis(500);
    loop {
        state.log(format!("observe: connecting to {} as {}", addr, username));
        let stream = match TcpStream::connect(&addr).await {
            Ok(s) => s,
            Err(e) => {
                state.log(format!("observe: connect failed: {}", e));
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(10));
                continue;
            }
        };
        stream.set_nodelay(true).ok();
        backoff = Duration::from_millis(500);
        let (mut r, mut w) = stream.into_split();
        if w.write_all(username.as_bytes()).await.is_err() {
            state.log("observe: handshake failed".to_string());
            continue;
        }
        state.log(format!("observe: connected to {}", addr));

        // Poll the backend for users/rooms so the dashboard panels stay current.
        // Send one command per tick (alternating) to avoid them coalescing into a
        // single recv on the backend, which frames one message per read.
        let mut refresh = tokio::time::interval(Duration::from_secs(2));
        let mut ask_rooms = false;
        let mut collector = ObserveCollector::new(username.clone());
        let mut buf = [0u8; 2048];

        loop {
            tokio::select! {
                _ = refresh.tick() => {
                    let cmd = if ask_rooms { "/rooms" } else { "/list" };
                    ask_rooms = !ask_rooms;
                    if w.write_all(&xor_bytes(cmd.as_bytes())).await.is_err() {
                        break;
                    }
                }
                cmd = admin_rx.recv() => {
                    match cmd {
                        Some(c) => {
                            let wire = format!("/admin {}", c.trim());
                            if w.write_all(&xor_bytes(wire.as_bytes())).await.is_err() {
                                break;
                            }
                            state.log(format!("[admin] sent: {}", c.trim()));
                        }
                        None => return,
                    }
                }
                res = r.read(&mut buf) => {
                    match res {
                        Ok(0) | Err(_) => {
                            state.log("observe: upstream closed".to_string());
                            break;
                        }
                        Ok(n) => {
                            let decrypted = xor_bytes(&buf[..n]);
                            let text = String::from_utf8_lossy(&decrypted).to_string();
                            for raw in text.split('\n') {
                                let line = raw.trim_end_matches('\r');
                                if line.is_empty() {
                                    continue;
                                }
                                collector.feed(line, &state);
                            }
                            collector.flush(&state);
                        }
                    }
                }
            }
        }

        tokio::time::sleep(Duration::from_millis(800)).await;
    }
}

/// Reassembles `/list` and `/rooms` replies from the backend's text stream into
/// the dashboard's users/rooms panels. Lines outside those blocks go to the
/// activity log.
struct ObserveCollector {
    self_name: String,
    mode: ObserveMode,
    users: Vec<(String, String)>,
    rooms: Vec<(String, usize)>,
}

#[derive(PartialEq)]
enum ObserveMode {
    None,
    Users,
    Rooms,
}

impl ObserveCollector {
    fn new(self_name: String) -> Self {
        Self { self_name, mode: ObserveMode::None, users: Vec::new(), rooms: Vec::new() }
    }

    fn feed(&mut self, line: &str, state: &Arc<ServerState>) {
        let trimmed = line.trim();
        if trimmed == shared::USER_LIST_HEADER {
            self.flush(state);
            self.mode = ObserveMode::Users;
            self.users.clear();
            return;
        }
        if trimmed == shared::ROOM_LIST_HEADER {
            self.flush(state);
            self.mode = ObserveMode::Rooms;
            self.rooms.clear();
            return;
        }
        match self.mode {
            ObserveMode::Users => {
                if let Some(item) = shared::list_item(line) {
                    let (name, room) = shared::parse_user_entry(item);
                    self.users.push((name, room.unwrap_or_default()));
                    return;
                }
            }
            ObserveMode::Rooms => {
                if let Some(item) = shared::list_item(line) {
                    let (room, count) = shared::parse_room_entry(item);
                    self.rooms.push((room, count.unwrap_or(0)));
                    return;
                }
            }
            ObserveMode::None => {}
        }
        // Not part of a list: end any open block, then log the line as activity.
        self.flush(state);
        state.bump_msg_count();
        state.log(line.to_string());
    }

    fn flush(&mut self, state: &Arc<ServerState>) {
        match std::mem::replace(&mut self.mode, ObserveMode::None) {
            ObserveMode::Users => {
                // Hide the observer's own connection from the roster.
                let users: Vec<_> = self
                    .users
                    .drain(..)
                    .filter(|(n, _)| n != &self.self_name)
                    .collect();
                state.set_observed_users(users);
            }
            ObserveMode::Rooms => {
                let rooms = std::mem::take(&mut self.rooms);
                state.set_observed_rooms(rooms);
            }
            ObserveMode::None => {}
        }
    }
}
