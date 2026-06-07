use serde::{Deserialize, Serialize};

pub const DEFAULT_PORT: u16 = 8080;
pub const PROTOCOL_VERSION: u32 = 1;
pub const DEFAULT_ROOM: &str = "general";
pub const MAX_MESSAGE_BYTES: usize = 8 * 1024;
pub const XOR_KEY: &[u8] = b"VectorcomSecureKey2025";

pub fn xor_bytes(data: &[u8]) -> Vec<u8> {
    data.iter()
        .enumerate()
        .map(|(i, b)| b ^ XOR_KEY[i % XOR_KEY.len()])
        .collect()
}

/// Header line that precedes a user listing (emitted by both servers in reply
/// to `/list`). Used to reconstruct structured lists from the text protocol.
pub const USER_LIST_HEADER: &str = "Online users:";
/// Header line that precedes a room listing (reply to `/rooms`).
pub const ROOM_LIST_HEADER: &str = "Active Rooms:";

/// If `line` is a " - <item>" list entry, return `<item>` trimmed; else `None`.
pub fn list_item(line: &str) -> Option<&str> {
    line.trim_start().strip_prefix("- ").map(str::trim)
}

/// Parse a user-list entry: "alice" or "alice (room: general)".
pub fn parse_user_entry(item: &str) -> (String, Option<String>) {
    if let Some(idx) = item.find(" (room: ") {
        let name = item[..idx].trim().to_string();
        let room = item[idx + " (room: ".len()..].trim_end_matches(')').trim().to_string();
        (name, Some(room))
    } else {
        (item.trim().to_string(), None)
    }
}

/// Parse a room-list entry: "general" or "general (2 users)".
pub fn parse_room_entry(item: &str) -> (String, Option<usize>) {
    if let Some(idx) = item.find(" (") {
        let name = item[..idx].trim().to_string();
        let count = item[idx + 2..]
            .split_whitespace()
            .next()
            .and_then(|n| n.parse::<usize>().ok());
        (name, count)
    } else {
        (item.trim().to_string(), None)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ClientMessage {
    Hello { username: String, version: u32 },
    Say { text: String },
    Join { room: String },
    ListUsers,
    ListRooms,
    Pm { to: String, text: String },
    Block { username: String },
    Unblock { username: String },
    Quit,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ServerMessage {
    Welcome { username: String, room: String, motd: String },
    Chat { from: String, room: String, text: String, ts: i64 },
    Pm { from: String, text: String, ts: i64 },
    System { text: String, ts: i64 },
    Error { text: String },
    UserList { users: Vec<String> },
    RoomList { rooms: Vec<String> },
    Joined { room: String },
    UserJoined { username: String, room: String },
    UserLeft { username: String, room: String },
}

pub fn now_ts() -> i64 {
    chrono::Utc::now().timestamp()
}

pub fn validate_username(name: &str) -> Result<(), &'static str> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("username cannot be empty");
    }
    if trimmed.len() > 63 {
        return Err("username too long (max 63 chars)");
    }
    if trimmed.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("username may not contain whitespace or control characters");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_item_matches_only_dash_entries() {
        assert_eq!(list_item(" - alice"), Some("alice"));
        assert_eq!(list_item("   - bob (room: x)"), Some("bob (room: x)"));
        assert_eq!(list_item("Online users:"), None);
        assert_eq!(list_item("------------"), None); // history divider, not an entry
        assert_eq!(list_item("alice: hi - there"), None);
    }

    #[test]
    fn parses_user_entries_with_and_without_room() {
        assert_eq!(parse_user_entry("alice"), ("alice".into(), None));
        assert_eq!(
            parse_user_entry("alice (room: general)"),
            ("alice".into(), Some("general".into()))
        );
    }

    #[test]
    fn parses_room_entries_with_and_without_count() {
        assert_eq!(parse_room_entry("general"), ("general".into(), None));
        assert_eq!(parse_room_entry("general (3 users)"), ("general".into(), Some(3)));
    }
}

pub fn validate_room(name: &str) -> Result<(), &'static str> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("room cannot be empty");
    }
    if trimmed.len() > 32 {
        return Err("room name too long");
    }
    if !trimmed.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return Err("room may only contain letters, digits, '_' and '-'");
    }
    Ok(())
}
