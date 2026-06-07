use std::collections::VecDeque;

use crate::net::ConnStatus;

pub struct App {
    pub username: String,
    pub server_addr: String,
    pub current_room: String,
    pub online: Vec<String>,
    pub history: VecDeque<ChatLine>,
    pub input: Input,
    pub scroll_offset: usize,
    pub connection: ConnStatus,
}

impl App {
    pub fn new(username: String, server_addr: String) -> Self {
        Self {
            username,
            server_addr,
            current_room: "general".into(),
            online: Vec::new(),
            history: VecDeque::with_capacity(1024),
            input: Input::default(),
            scroll_offset: 0,
            connection: ConnStatus::Connecting,
        }
    }

    pub fn push(&mut self, line: ChatLine) {
        if self.history.len() >= 2000 {
            self.history.pop_front();
        }
        self.history.push_back(line);
        if self.scroll_offset > 0 {
            self.scroll_offset = self.scroll_offset.saturating_add(1);
        }
    }

    pub fn scroll_up(&mut self) {
        self.scroll_offset = (self.scroll_offset + 1).min(self.history.len());
    }
    pub fn scroll_down(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_sub(1);
    }
    pub fn page_up(&mut self) {
        self.scroll_offset = (self.scroll_offset + 10).min(self.history.len());
    }
    pub fn page_down(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_sub(10);
    }
}

#[derive(Debug, Clone)]
pub struct ChatLine {
    pub kind: LineKind,
    pub text: String,
    pub ts: i64,
}

impl ChatLine {
    pub fn system(text: String) -> Self {
        Self { kind: LineKind::System, text, ts: shared::now_ts() }
    }
    pub fn error(text: String) -> Self {
        Self { kind: LineKind::Error, text, ts: shared::now_ts() }
    }
}

#[derive(Debug, Clone)]
pub enum LineKind {
    Chat { from: String },
    Pm { from: String, outgoing: bool },
    System,
    Error,
}

#[derive(Default)]
pub struct Input {
    pub buf: String,
    pub cursor: usize,
}

impl Input {
    pub fn insert(&mut self, c: char) {
        self.buf.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }
    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let prev = prev_char_boundary(&self.buf, self.cursor);
        self.buf.replace_range(prev..self.cursor, "");
        self.cursor = prev;
    }
    pub fn delete(&mut self) {
        if self.cursor >= self.buf.len() {
            return;
        }
        let next = next_char_boundary(&self.buf, self.cursor);
        self.buf.replace_range(self.cursor..next, "");
    }
    pub fn left(&mut self) {
        if self.cursor > 0 {
            self.cursor = prev_char_boundary(&self.buf, self.cursor);
        }
    }
    pub fn right(&mut self) {
        if self.cursor < self.buf.len() {
            self.cursor = next_char_boundary(&self.buf, self.cursor);
        }
    }
    pub fn home(&mut self) { self.cursor = 0; }
    pub fn end(&mut self) { self.cursor = self.buf.len(); }
    pub fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.buf)
    }
}

fn prev_char_boundary(s: &str, i: usize) -> usize {
    let mut j = i.saturating_sub(1);
    while j > 0 && !s.is_char_boundary(j) {
        j -= 1;
    }
    j
}
fn next_char_boundary(s: &str, i: usize) -> usize {
    let mut j = (i + 1).min(s.len());
    while j < s.len() && !s.is_char_boundary(j) {
        j += 1;
    }
    j
}
