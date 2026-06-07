use std::collections::{BTreeMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use shared::{now_ts, validate_room, ServerMessage, DEFAULT_ROOM};
use tokio::sync::mpsc::Sender;

#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Debug)]
pub struct ClientId(pub u64);

pub struct Client {
    pub username: String,
    pub room: String,
    pub tx: Sender<ServerMessage>,
    pub blocked: HashSet<String>,
}

pub struct ServerState {
    next_id: AtomicU64,
    inner: Mutex<Inner>,
}

struct Inner {
    clients: BTreeMap<ClientId, Client>,
    name_to_id: BTreeMap<String, ClientId>,
    rooms: BTreeMap<String, HashSet<ClientId>>,
    log: VecDeque<LogLine>,
    total_messages: u64,
    total_connections: u64,
    uptime: Duration,
    // Observe mode: the dashboard isn't the authoritative server, so users/rooms
    // are reconstructed from an upstream backend's `/list` and `/rooms` replies.
    observe_mode: bool,
    observed_users: Vec<(String, String)>,
    observed_rooms: Vec<(String, usize)>,
}

#[derive(Clone)]
pub struct LogLine {
    pub ts: i64,
    pub text: String,
}

impl ServerState {
    pub fn new() -> Self {
        let mut rooms = BTreeMap::new();
        rooms.insert(DEFAULT_ROOM.to_string(), HashSet::new());
        Self {
            next_id: AtomicU64::new(1),
            inner: Mutex::new(Inner {
                clients: BTreeMap::new(),
                name_to_id: BTreeMap::new(),
                rooms,
                log: VecDeque::with_capacity(512),
                total_messages: 0,
                total_connections: 0,
                uptime: Duration::ZERO,
                observe_mode: false,
                observed_users: Vec::new(),
                observed_rooms: Vec::new(),
            }),
        }
    }

    pub fn set_observe_mode(&self, on: bool) {
        self.inner.lock().unwrap().observe_mode = on;
    }

    pub fn set_observed_users(&self, users: Vec<(String, String)>) {
        self.inner.lock().unwrap().observed_users = users;
    }

    pub fn set_observed_rooms(&self, rooms: Vec<(String, usize)>) {
        self.inner.lock().unwrap().observed_rooms = rooms;
    }

    /// Broadcast a server notice to the default room (used by `say` admin
    /// command when running as a standalone Rust server).
    pub fn admin_say(&self, text: String) {
        self.broadcast_room(
            DEFAULT_ROOM,
            ServerMessage::System { text: format!("[SERVER] {}", text), ts: now_ts() },
            None,
        );
    }

    pub fn log(&self, text: impl Into<String>) {
        let mut g = self.inner.lock().unwrap();
        if g.log.len() >= 500 {
            g.log.pop_front();
        }
        g.log.push_back(LogLine { ts: now_ts(), text: text.into() });
    }

    pub fn bump_msg_count(&self) {
        self.inner.lock().unwrap().total_messages += 1;
    }

    pub fn set_uptime(&self, d: Duration) {
        self.inner.lock().unwrap().uptime = d;
    }

    pub fn snapshot(&self) -> Snapshot {
        let g = self.inner.lock().unwrap();
        let (rooms, users) = if g.observe_mode {
            (g.observed_rooms.clone(), g.observed_users.clone())
        } else {
            let rooms = g.rooms.iter().map(|(k, v)| (k.clone(), v.len())).collect();
            let users = g.clients.values()
                .map(|c| (c.username.clone(), c.room.clone()))
                .collect();
            (rooms, users)
        };
        Snapshot {
            log: g.log.iter().cloned().collect(),
            rooms,
            users,
            total_messages: g.total_messages,
            total_connections: g.total_connections,
            uptime: g.uptime,
        }
    }

    pub fn register(&self, username: String, tx: Sender<ServerMessage>) -> Result<ClientId, &'static str> {
        let mut g = self.inner.lock().unwrap();
        if g.name_to_id.contains_key(&username) {
            return Err("username already in use");
        }
        let id = ClientId(self.next_id.fetch_add(1, Ordering::Relaxed));
        let client = Client {
            username: username.clone(),
            room: DEFAULT_ROOM.into(),
            tx,
            blocked: HashSet::new(),
        };
        g.clients.insert(id, client);
        g.name_to_id.insert(username, id);
        g.rooms.entry(DEFAULT_ROOM.into()).or_default().insert(id);
        g.total_connections += 1;
        Ok(id)
    }

    pub fn unregister(&self, id: ClientId) {
        let mut g = self.inner.lock().unwrap();
        if let Some(c) = g.clients.remove(&id) {
            g.name_to_id.remove(&c.username);
            if let Some(set) = g.rooms.get_mut(&c.room) {
                set.remove(&id);
            }
            prune_empty_room(&mut g, &c.room);
            let room = c.room.clone();
            let username = c.username.clone();
            drop(g);
            self.broadcast_room(&room, ServerMessage::UserLeft { username, room: room.clone() }, Some(id));
        }
    }

    pub fn send_to(&self, id: ClientId, msg: ServerMessage) {
        let g = self.inner.lock().unwrap();
        if let Some(c) = g.clients.get(&id) {
            let _ = c.tx.try_send(msg);
        }
    }

    pub fn broadcast_room(&self, room: &str, msg: ServerMessage, except: Option<ClientId>) {
        let g = self.inner.lock().unwrap();
        if let Some(members) = g.rooms.get(room) {
            for &mid in members {
                if Some(mid) == except {
                    continue;
                }
                if let Some(c) = g.clients.get(&mid) {
                    if let ServerMessage::Chat { from, .. } = &msg {
                        if c.blocked.contains(from) {
                            continue;
                        }
                    }
                    let _ = c.tx.try_send(msg.clone());
                }
            }
        }
    }

    pub fn handle_say(&self, id: ClientId, text: String) {
        let (room, from) = {
            let g = self.inner.lock().unwrap();
            match g.clients.get(&id) {
                Some(c) => (c.room.clone(), c.username.clone()),
                None => return,
            }
        };
        let msg = ServerMessage::Chat { from, room: room.clone(), text, ts: now_ts() };
        // Exclude the sender: the client echoes its own message locally, so
        // echoing it back here would display it twice.
        self.broadcast_room(&room, msg, Some(id));
    }

    pub fn handle_join(&self, id: ClientId, new_room: String) {
        if let Err(e) = validate_room(&new_room) {
            self.send_to(id, ServerMessage::Error { text: e.into() });
            return;
        }
        let (old_room, username) = {
            let mut g = self.inner.lock().unwrap();
            let c = match g.clients.get_mut(&id) {
                Some(c) => c,
                None => return,
            };
            let old = c.room.clone();
            if old == new_room {
                return;
            }
            c.room = new_room.clone();
            let username = c.username.clone();
            if let Some(set) = g.rooms.get_mut(&old) {
                set.remove(&id);
            }
            prune_empty_room(&mut g, &old);
            g.rooms.entry(new_room.clone()).or_default().insert(id);
            (old, username)
        };
        self.broadcast_room(&old_room, ServerMessage::UserLeft { username: username.clone(), room: old_room.clone() }, Some(id));
        self.send_to(id, ServerMessage::Joined { room: new_room.clone() });
        self.broadcast_room(&new_room.clone(), ServerMessage::UserJoined { username, room: new_room }, Some(id));
    }

    pub fn handle_list_users(&self, id: ClientId) {
        let users: Vec<String> = {
            let g = self.inner.lock().unwrap();
            let room = match g.clients.get(&id) {
                Some(c) => c.room.clone(),
                None => return,
            };
            g.rooms.get(&room)
                .map(|s| s.iter().filter_map(|i| g.clients.get(i).map(|c| c.username.clone())).collect())
                .unwrap_or_default()
        };
        self.send_to(id, ServerMessage::UserList { users });
    }

    pub fn handle_list_rooms(&self, id: ClientId) {
        let rooms: Vec<String> = {
            let g = self.inner.lock().unwrap();
            g.rooms.keys().cloned().collect()
        };
        self.send_to(id, ServerMessage::RoomList { rooms });
    }

    pub fn handle_pm(&self, id: ClientId, to: String, text: String) {
        let (from_name, target_id) = {
            let g = self.inner.lock().unwrap();
            let from = match g.clients.get(&id) {
                Some(c) => c.username.clone(),
                None => return,
            };
            let target = g.name_to_id.get(&to).copied();
            (from, target)
        };
        match target_id {
            Some(tid) => {
                let g = self.inner.lock().unwrap();
                if let Some(target) = g.clients.get(&tid) {
                    if target.blocked.contains(&from_name) {
                        drop(g);
                        self.send_to(id, ServerMessage::Error { text: format!("you are blocked by {}", to) });
                        return;
                    }
                    let _ = target.tx.try_send(ServerMessage::Pm { from: from_name, text, ts: now_ts() });
                }
            }
            None => self.send_to(id, ServerMessage::Error { text: format!("user '{}' not found", to) }),
        }
    }

    pub fn handle_block(&self, id: ClientId, target: String, block: bool) {
        let mut g = self.inner.lock().unwrap();
        let c = match g.clients.get_mut(&id) {
            Some(c) => c,
            None => return,
        };
        if block {
            c.blocked.insert(target.clone());
        } else {
            c.blocked.remove(&target);
        }
        let tx = c.tx.clone();
        let _ = tx.try_send(ServerMessage::System {
            text: format!("{} {}", if block { "blocked" } else { "unblocked" }, target),
            ts: now_ts(),
        });
    }
}

/// Remove a room from the registry once it has no members, so `/rooms` and the
/// dashboard don't accumulate ghost rooms. The default room is kept permanently.
fn prune_empty_room(inner: &mut Inner, room: &str) {
    if room != DEFAULT_ROOM && inner.rooms.get(room).is_some_and(|s| s.is_empty()) {
        inner.rooms.remove(room);
    }
}

pub struct Snapshot {
    pub log: Vec<LogLine>,
    pub rooms: Vec<(String, usize)>,
    pub users: Vec<(String, String)>,
    pub total_messages: u64,
    pub total_connections: u64,
    pub uptime: Duration,
}
