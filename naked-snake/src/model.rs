//! Core domain types and constants shared across the application:
//! validated game configuration, coordinates, directions and limits from
//! `snakes_task.md`.

use crate::proto;

pub const MULTICAST_ADDR: std::net::Ipv4Addr = std::net::Ipv4Addr::new(239, 192, 0, 4);
pub const MULTICAST_PORT: u16 = 9193;
pub const PROTOCOL_VERSION: u32 = 2;

pub const MAX_DATAGRAM_BYTES: usize = 1200;
pub const MAX_PLAYERS: usize = 6;
pub const MAX_SNAKES: usize = 4;
#[allow(dead_code)]
pub const MIN_SNAKE_LEN: usize = 2;
pub const MAX_SNAKE_LEN: usize = 32;
pub const MAX_PLAYER_NAME_BYTES: usize = 24;
pub const MAX_GAME_NAME_BYTES: usize = 32;
pub const MAX_ERROR_BYTES: usize = 128;

pub const MIN_FIELD_SIZE: i32 = 10;
pub const MAX_FIELD_SIZE: i32 = 100;
pub const MIN_FOOD_STATIC: i32 = 0;
pub const MAX_FOOD_STATIC: i32 = 10;
pub const MIN_STATE_DELAY_MS: i32 = 100;
pub const MAX_STATE_DELAY_MS: i32 = 3000;

pub const RESEND_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);
pub const PING_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);
pub const PEER_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1000);
pub const ANNOUNCE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(1000);
pub const ANNOUNCE_TTL: std::time::Duration = std::time::Duration::from_millis(3000);
pub const DISCOVER_INTERVAL_WITH_CONFIG: std::time::Duration = std::time::Duration::from_millis(500);
pub const DISCOVER_INTERVAL_BROWSING: std::time::Duration = std::time::Duration::from_millis(1000);
pub const JOIN_TOTAL_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(5000);
#[allow(dead_code)]
pub const REJOIN_ANNOUNCE_WINDOW: std::time::Duration = std::time::Duration::from_millis(3000);
pub const DEDUP_WINDOW: i64 = 1024;
pub const MAX_UNACKED_PER_PEER: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

impl Point {
    pub fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    Up,
    Down,
    Left,
    Right,
}

impl Direction {
    pub fn opposite(self) -> Direction {
        match self {
            Direction::Up => Direction::Down,
            Direction::Down => Direction::Up,
            Direction::Left => Direction::Right,
            Direction::Right => Direction::Left,
        }
    }

    pub fn delta(self) -> (i32, i32) {
        match self {
            Direction::Up => (0, -1),
            Direction::Down => (0, 1),
            Direction::Left => (-1, 0),
            Direction::Right => (1, 0),
        }
    }

    pub fn to_proto(self) -> proto::Direction {
        match self {
            Direction::Up => proto::Direction::UP,
            Direction::Down => proto::Direction::DOWN,
            Direction::Left => proto::Direction::LEFT,
            Direction::Right => proto::Direction::RIGHT,
        }
    }

    pub fn from_proto(d: proto::Direction) -> Direction {
        match d {
            proto::Direction::UP => Direction::Up,
            proto::Direction::DOWN => Direction::Down,
            proto::Direction::LEFT => Direction::Left,
            proto::Direction::RIGHT => Direction::Right,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GameConfig {
    pub width: i32,
    pub height: i32,
    pub food_static: i32,
    pub state_delay_ms: i32,
}

impl Default for GameConfig {
    fn default() -> Self {
        Self {
            width: 40,
            height: 30,
            food_static: 1,
            state_delay_ms: 1000,
        }
    }
}

impl GameConfig {
    /// Validates and clamps a config coming over the network; missing fields
    /// already carry schema defaults from the generated accessors.
    pub fn from_proto(c: &proto::GameConfig) -> GameConfig {
        GameConfig {
            width: c.width().clamp(MIN_FIELD_SIZE, MAX_FIELD_SIZE),
            height: c.height().clamp(MIN_FIELD_SIZE, MAX_FIELD_SIZE),
            food_static: c.food_static().clamp(MIN_FOOD_STATIC, MAX_FOOD_STATIC),
            state_delay_ms: c
                .state_delay_ms()
                .clamp(MIN_STATE_DELAY_MS, MAX_STATE_DELAY_MS),
        }
    }

    pub fn to_proto(self) -> proto::GameConfig {
        let mut c = proto::GameConfig::new();
        c.set_width(self.width);
        c.set_height(self.height);
        c.set_food_static(self.food_static);
        c.set_state_delay_ms(self.state_delay_ms);
        c
    }

    pub fn is_valid(&self) -> bool {
        (MIN_FIELD_SIZE..=MAX_FIELD_SIZE).contains(&self.width)
            && (MIN_FIELD_SIZE..=MAX_FIELD_SIZE).contains(&self.height)
            && (MIN_FOOD_STATIC..=MAX_FOOD_STATIC).contains(&self.food_static)
            && (MIN_STATE_DELAY_MS..=MAX_STATE_DELAY_MS).contains(&self.state_delay_ms)
    }

    pub fn wrap(&self, p: Point) -> Point {
        Point::new(p.x.rem_euclid(self.width), p.y.rem_euclid(self.height))
    }
}

pub fn truncate_utf8(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}
