use chrono::Local;
use shared::{ClientMessage, ServerMessage};

fn ts() -> String {
    Local::now().format("%H:%M:%S").to_string()
}

pub fn render_server(msg: &ServerMessage) -> String {
    match msg {
        ServerMessage::Welcome { motd, room, .. } => {
            format!("[SERVER] {} (room: {})\n", motd, room)
        }
        ServerMessage::Chat { from, text, .. } => {
            format!("[{}] {}: {}\n", ts(), from, text)
        }
        ServerMessage::Pm { from, text, .. } => {
            format!("[PM from {}] {}\n", from, text)
        }
        ServerMessage::System { text, .. } => format!("{}\n", text),
        ServerMessage::Error { text } => format!("Error: {}\n", text),
        ServerMessage::UserList { users } => {
            let mut s = String::from("Online users:\n");
            for u in users {
                s.push_str(&format!(" - {}\n", u));
            }
            s
        }
        ServerMessage::RoomList { rooms } => {
            let mut s = String::from("Active Rooms:\n");
            for r in rooms {
                s.push_str(&format!(" - {}\n", r));
            }
            s
        }
        ServerMessage::Joined { room } => format!("Joined #{}\n", room),
        ServerMessage::UserJoined { username, room } => {
            format!("[{}] {} joined room {}\n", ts(), username, room)
        }
        ServerMessage::UserLeft { username, room } => {
            format!("[{}] {} left room {}\n", ts(), username, room)
        }
    }
}

pub fn parse_client(line: &str) -> ClientMessage {
    let t = line.trim();
    if t == "/list" || t == "/users" {
        return ClientMessage::ListUsers;
    }
    if t == "/rooms" {
        return ClientMessage::ListRooms;
    }
    if t == "/quit" || t == "/exit" {
        return ClientMessage::Quit;
    }
    if let Some(r) = t.strip_prefix("/join ") {
        return ClientMessage::Join { room: r.trim().to_string() };
    }
    if let Some(r) = t.strip_prefix("/block ") {
        return ClientMessage::Block { username: r.trim().to_string() };
    }
    if let Some(r) = t.strip_prefix("/unblock ") {
        return ClientMessage::Unblock { username: r.trim().to_string() };
    }
    if let Some(rest) = t.strip_prefix("/pm ").or_else(|| t.strip_prefix("/msg ")) {
        if let Some(sp) = rest.find(' ') {
            let to = rest[..sp].to_string();
            let text = rest[sp + 1..].to_string();
            if !to.is_empty() && !text.is_empty() {
                return ClientMessage::Pm { to, text };
            }
        }
    }
    ClientMessage::Say { text: line.to_string() }
}
