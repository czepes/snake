//! Сообщения протокола v2.
//!
//! Проводной формат генерируется `prost` из `protobuf/snakes.proto`
//! (см. `build.rs`). Доменные типы (`Dir`, `Config`, `PlayerInfo`, `Body`, …)
//! остаются здесь же: они удобнее для ядра игры, а конверсии в `proto::*`
//! и обратно сосредоточены в этом модуле.
//!
//! [`decode`] проверяет protobuf, версию, номер сообщения и представимость полей
//! в доменных типах. Это не полная проверка протокола: принадлежность игре,
//! полномочия отправителя и смысл снимка разбирает [`crate::node`], а размер
//! входящей датаграммы ограничивает [`crate::net`]. При обратной конверсии
//! protobuf собирается заново, поэтому неизвестные поля не попадают в исходящие снимки.

use std::net::{Ipv4Addr, SocketAddrV4};
use std::str::FromStr;

use prost::Message as _;

pub mod proto {
    include!("snakes.v2.rs");
}

pub const PROTOCOL_VERSION: u32 = 2;
pub const MAX_DATAGRAM: usize = 1200;
pub const MAX_PLAYER_NAME: usize = 24;
pub const MAX_GAME_NAME: usize = 32;
pub const MAX_ERROR_TEXT: usize = 128;
pub const MAX_PLAYERS: usize = 6;
pub const MAX_SNAKES: usize = 4;

// ───────────────────────────── Доменные типы ─────────────────────────────────

/// Сетевая обязанность узла, не зависящая от наличия живой змейки.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeRole {
    Normal,
    Master,
    Deputy,
}

/// Участие в игре: управлять змейкой или наблюдать, сохраняя сетевую роль.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayerStatus {
    Playing,
    Viewing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayerType {
    Human,
    Robot,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Dir {
    Up,
    Down,
    Left,
    Right,
}

impl Dir {
    pub fn opposite(self) -> Dir {
        match self {
            Dir::Up => Dir::Down,
            Dir::Down => Dir::Up,
            Dir::Left => Dir::Right,
            Dir::Right => Dir::Left,
        }
    }

    pub fn delta(self) -> (i32, i32) {
        match self {
            Dir::Up => (0, -1),
            Dir::Down => (0, 1),
            Dir::Left => (-1, 0),
            Dir::Right => (1, 0),
        }
    }
}

/// Участник сессии: ID, endpoint, роль, статус и накопленные очки.
#[derive(Clone, Debug, PartialEq)]
pub struct PlayerInfo {
    pub name: String,
    pub id: i32,
    /// В сообщении отправитель опускает свой endpoint: его даёт UDP-пакет.
    pub addr: Option<SocketAddrV4>,
    pub role: NodeRole,
    pub ptype: PlayerType,
    pub score: i32,
    pub status: PlayerStatus,
}

/// Неизменяемые параметры сессии; значения по умолчанию совпадают со схемой.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    pub width: i32,
    pub height: i32,
    pub food_static: i32,
    pub state_delay_ms: i32,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            width: 40,
            height: 30,
            food_static: 1,
            state_delay_ms: 1000,
        }
    }
}

impl Config {
    /// Проверяет диапазоны из задания; protobuf сам их не ограничивает.
    pub fn is_valid(&self) -> bool {
        (10..=100).contains(&self.width)
            && (10..=100).contains(&self.height)
            && (0..=10).contains(&self.food_static)
            && (100..=3000).contains(&self.state_delay_ms)
    }
}

/// Компактное тело змейки: абсолютная голова, затем относительные осевые отрезки.
/// Смещения описывают реальный путь тела, даже если он несколько раз пересекает край.
#[derive(Clone, Debug, PartialEq)]
pub struct WireSnake {
    pub player_id: i32,
    pub points: Vec<(i32, i32)>,
    pub dir: Dir,
}

/// Полный снимок мира от MASTER, а не набор изменений с прошлого хода.
#[derive(Clone, Debug, PartialEq)]
pub struct StateMsg {
    /// Версия игрового состояния, независимая от номера UDP-сообщения.
    pub order: i32,
    pub snakes: Vec<WireSnake>,
    pub foods: Vec<(i32, i32)>,
    pub players: Vec<PlayerInfo>,
    /// Следующий ещё не выданный ID; наследуется при смене MASTER.
    pub next_player_id: i32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Announcement {
    pub players: Vec<PlayerInfo>,
    pub config: Config,
    pub can_join: bool,
    pub game_name: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Body {
    Ping,
    Steer(Dir),
    Ack,
    State(StateMsg),
    Announcement(Vec<Announcement>),
    Join {
        ptype: PlayerType,
        player_name: String,
        game_name: String,
        status: PlayerStatus,
    },
    Error {
        text: String,
        request_seq: i64,
    },
    RoleChange {
        sender: Option<NodeRole>,
        receiver: Option<NodeRole>,
        order: i32,
    },
    Discover,
    StatusChange(PlayerStatus),
    Leave,
}

impl Body {
    pub fn name(&self) -> &'static str {
        match self {
            Body::Ping => "Ping",
            Body::Steer(_) => "Steer",
            Body::Ack => "Ack",
            Body::State(_) => "State",
            Body::Announcement(_) => "Announcement",
            Body::Join { .. } => "Join",
            Body::Error { .. } => "Error",
            Body::RoleChange { .. } => "RoleChange",
            Body::Discover => "Discover",
            Body::StatusChange(_) => "StatusChange",
            Body::Leave => "Leave",
        }
    }

    pub fn needs_ack(&self) -> bool {
        !matches!(self, Body::Announcement(_) | Body::Discover | Body::Ack)
    }
}

/// Оболочка одной датаграммы; `protocol_version` добавляется при кодировании.
#[derive(Clone, Debug, PartialEq)]
pub struct Message {
    /// Номер сообщения отправителя; у ACK копирует номер подтверждаемого запроса.
    pub seq: i64,
    pub sender_id: Option<i32>,
    pub receiver_id: Option<i32>,
    pub game_name: Option<String>,
    pub body: Body,
}

#[derive(Debug, PartialEq, Eq)]
pub struct DecodeError(pub &'static str);

#[derive(Debug, PartialEq, Eq)]
pub enum EncodeError {
    TooLarge(usize),
    Prost(&'static str),
}

// ──────────────────────────────── Кодирование ────────────────────────────────

impl Message {
    pub fn to_proto(&self) -> proto::GameMessage {
        use proto::game_message as gm;

        let ty = match &self.body {
            Body::Ping => gm::Type::Ping(gm::PingMsg {}),
            Body::Steer(d) => gm::Type::Steer(gm::SteerMsg {
                direction: dir_to_proto(*d) as i32,
            }),
            Body::Ack => gm::Type::Ack(gm::AckMsg {}),
            Body::State(s) => gm::Type::State(gm::StateMsg {
                state: state_to_proto(s),
            }),
            Body::Announcement(games) => gm::Type::Announcement(gm::AnnouncementMsg {
                games: games.iter().map(announcement_to_proto).collect(),
            }),
            Body::Join {
                ptype,
                player_name,
                game_name,
                status,
            } => gm::Type::Join(gm::JoinMsg {
                player_type: Some(ptype_to_proto(*ptype) as i32),
                player_name: player_name.clone(),
                game_name: game_name.clone(),
                requested_status: status_to_proto(*status) as i32,
            }),
            Body::Error { text, request_seq } => gm::Type::Error(gm::ErrorMsg {
                error_message: text.clone(),
                request_seq: *request_seq,
            }),
            Body::RoleChange {
                sender,
                receiver,
                order,
            } => gm::Type::NodeRoleChange(gm::NodeRoleChangeMsg {
                sender_role: sender.map(|r| role_to_proto(r) as i32),
                receiver_role: receiver.map(|r| role_to_proto(r) as i32),
                state_order: *order,
            }),
            Body::Discover => gm::Type::Discover(gm::DiscoverMsg {}),
            Body::StatusChange(s) => gm::Type::PlayerStatusChange(gm::PlayerStatusChangeMsg {
                status: status_to_proto(*s) as i32,
            }),
            Body::Leave => gm::Type::Leave(gm::LeaveMsg {}),
        };

        proto::GameMessage {
            msg_seq: self.seq,
            sender_id: self.sender_id,
            receiver_id: self.receiver_id,
            protocol_version: PROTOCOL_VERSION,
            game_name: self.game_name.clone(),
            r#type: Some(ty),
        }
    }

    pub fn from_proto(m: proto::GameMessage) -> Result<Message, DecodeError> {
        use proto::game_message as gm;
        let body = match m.r#type {
            Some(gm::Type::Ping(_)) => Body::Ping,
            Some(gm::Type::Steer(s)) => Body::Steer(dir_from_proto(s.direction)?),
            Some(gm::Type::Ack(_)) => Body::Ack,
            Some(gm::Type::State(s)) => Body::State(state_from_proto(s.state)?),
            Some(gm::Type::Announcement(a)) => Body::Announcement(
                a.games
                    .into_iter()
                    .map(announcement_from_proto)
                    .collect::<Result<_, _>>()?,
            ),
            Some(gm::Type::Join(j)) => Body::Join {
                ptype: j
                    .player_type
                    .map(ptype_from_proto)
                    .transpose()?
                    .unwrap_or(PlayerType::Human),
                player_name: j.player_name,
                game_name: j.game_name,
                status: status_from_proto(j.requested_status)?,
            },
            Some(gm::Type::Error(e)) => Body::Error {
                text: e.error_message,
                request_seq: e.request_seq,
            },
            Some(gm::Type::NodeRoleChange(r)) => Body::RoleChange {
                sender: r.sender_role.map(role_from_proto).transpose()?,
                receiver: r.receiver_role.map(role_from_proto).transpose()?,
                order: r.state_order,
            },
            Some(gm::Type::Discover(_)) => Body::Discover,
            Some(gm::Type::PlayerStatusChange(s)) => {
                Body::StatusChange(status_from_proto(s.status)?)
            }
            Some(gm::Type::Leave(_)) => Body::Leave,
            None => return Err(DecodeError("missing message type")),
        };
        Ok(Message {
            seq: m.msg_seq,
            sender_id: m.sender_id,
            receiver_id: m.receiver_id,
            game_name: m.game_name,
            body,
        })
    }
}

/// Кодирует ровно один GameMessage без обрамления и проверяет итоговый размер.
/// Слишком большое сообщение возвращается как ошибка, а не обрезается.
pub fn encode(m: &Message) -> Result<Vec<u8>, EncodeError> {
    let p = m.to_proto();
    let mut buf = Vec::with_capacity(128);
    p.encode(&mut buf)
        .map_err(|_| EncodeError::Prost("encode"))?;
    if buf.len() > MAX_DATAGRAM {
        return Err(EncodeError::TooLarge(buf.len()));
    }
    Ok(buf)
}

/// Разбирает protobuf v2 в доменные типы; не проверяет игровую сессию и полномочия.
/// Лимит размера входящего пакета проверяется транспортом до вызова этой функции.
pub fn decode(bytes: &[u8]) -> Result<Message, DecodeError> {
    let p = proto::GameMessage::decode(bytes).map_err(|_| DecodeError("protobuf decode"))?;
    if p.protocol_version != PROTOCOL_VERSION {
        return Err(DecodeError("unsupported protocol version"));
    }
    if p.msg_seq < 0 {
        return Err(DecodeError("negative msg_seq"));
    }
    Message::from_proto(p)
}

// ──────────────────────── Конверсии: домен <-> proto ─────────────────────────

fn role_to_proto(r: NodeRole) -> proto::NodeRole {
    match r {
        NodeRole::Normal => proto::NodeRole::Normal,
        NodeRole::Master => proto::NodeRole::Master,
        NodeRole::Deputy => proto::NodeRole::Deputy,
    }
}

fn role_from_proto(v: i32) -> Result<NodeRole, DecodeError> {
    proto::NodeRole::try_from(v)
        .map(|r| match r {
            proto::NodeRole::Normal => NodeRole::Normal,
            proto::NodeRole::Master => NodeRole::Master,
            proto::NodeRole::Deputy => NodeRole::Deputy,
        })
        .map_err(|_| DecodeError("bad role"))
}

fn status_to_proto(s: PlayerStatus) -> proto::PlayerStatus {
    match s {
        PlayerStatus::Playing => proto::PlayerStatus::Playing,
        PlayerStatus::Viewing => proto::PlayerStatus::Viewing,
    }
}

fn status_from_proto(v: i32) -> Result<PlayerStatus, DecodeError> {
    proto::PlayerStatus::try_from(v)
        .map(|s| match s {
            proto::PlayerStatus::Playing => PlayerStatus::Playing,
            proto::PlayerStatus::Viewing => PlayerStatus::Viewing,
        })
        .map_err(|_| DecodeError("bad status"))
}

fn ptype_to_proto(t: PlayerType) -> proto::PlayerType {
    match t {
        PlayerType::Human => proto::PlayerType::Human,
        PlayerType::Robot => proto::PlayerType::Robot,
    }
}

fn ptype_from_proto(v: i32) -> Result<PlayerType, DecodeError> {
    proto::PlayerType::try_from(v)
        .map(|t| match t {
            proto::PlayerType::Human => PlayerType::Human,
            proto::PlayerType::Robot => PlayerType::Robot,
        })
        .map_err(|_| DecodeError("bad player type"))
}

fn dir_to_proto(d: Dir) -> proto::Direction {
    match d {
        Dir::Up => proto::Direction::Up,
        Dir::Down => proto::Direction::Down,
        Dir::Left => proto::Direction::Left,
        Dir::Right => proto::Direction::Right,
    }
}

fn dir_from_proto(v: i32) -> Result<Dir, DecodeError> {
    proto::Direction::try_from(v)
        .map(|d| match d {
            proto::Direction::Up => Dir::Up,
            proto::Direction::Down => Dir::Down,
            proto::Direction::Left => Dir::Left,
            proto::Direction::Right => Dir::Right,
        })
        .map_err(|_| DecodeError("bad direction"))
}

fn player_to_proto(p: &PlayerInfo) -> proto::GamePlayer {
    proto::GamePlayer {
        name: p.name.clone(),
        id: p.id,
        ip_address: p.addr.map(|a| a.ip().to_string()),
        port: p.addr.map(|a| i32::from(a.port())),
        role: role_to_proto(p.role) as i32,
        r#type: Some(ptype_to_proto(p.ptype) as i32),
        score: p.score,
        status: status_to_proto(p.status) as i32,
    }
}

fn player_from_proto(p: proto::GamePlayer) -> Result<PlayerInfo, DecodeError> {
    let addr = match (p.ip_address, p.port) {
        (Some(ip), Some(port)) => {
            let ip = Ipv4Addr::from_str(&ip).map_err(|_| DecodeError("bad ip"))?;
            let port = u16::try_from(port).map_err(|_| DecodeError("bad port"))?;
            if port == 0 {
                return Err(DecodeError("bad port"));
            }
            Some(SocketAddrV4::new(ip, port))
        }
        (None, None) => None,
        _ => return Err(DecodeError("ip without port")),
    };
    Ok(PlayerInfo {
        name: p.name,
        id: p.id,
        addr,
        role: role_from_proto(p.role)?,
        ptype: p
            .r#type
            .map(ptype_from_proto)
            .transpose()?
            .unwrap_or(PlayerType::Human),
        score: p.score,
        status: status_from_proto(p.status)?,
    })
}

fn config_to_proto(c: &Config) -> proto::GameConfig {
    proto::GameConfig {
        width: Some(c.width),
        height: Some(c.height),
        food_static: Some(c.food_static),
        state_delay_ms: Some(c.state_delay_ms),
    }
}

fn config_from_proto(c: proto::GameConfig) -> Config {
    let d = Config::default();
    Config {
        width: c.width.unwrap_or(d.width),
        height: c.height.unwrap_or(d.height),
        food_static: c.food_static.unwrap_or(d.food_static),
        state_delay_ms: c.state_delay_ms.unwrap_or(d.state_delay_ms),
    }
}

fn coord_to_proto((x, y): (i32, i32)) -> proto::game_state::Coord {
    proto::game_state::Coord {
        x: if x != 0 { Some(x) } else { None },
        y: if y != 0 { Some(y) } else { None },
    }
}

fn coord_from_proto(c: proto::game_state::Coord) -> (i32, i32) {
    (c.x.unwrap_or(0), c.y.unwrap_or(0))
}

fn snake_to_proto(s: &WireSnake) -> proto::game_state::Snake {
    proto::game_state::Snake {
        player_id: s.player_id,
        points: s.points.iter().map(|&p| coord_to_proto(p)).collect(),
        head_direction: dir_to_proto(s.dir) as i32,
    }
}

fn snake_from_proto(s: proto::game_state::Snake) -> Result<WireSnake, DecodeError> {
    Ok(WireSnake {
        player_id: s.player_id,
        points: s.points.into_iter().map(coord_from_proto).collect(),
        dir: dir_from_proto(s.head_direction)?,
    })
}

fn players_to_proto(players: &[PlayerInfo]) -> proto::GamePlayers {
    proto::GamePlayers {
        players: players.iter().map(player_to_proto).collect(),
    }
}

fn players_from_proto(p: proto::GamePlayers) -> Result<Vec<PlayerInfo>, DecodeError> {
    if p.players.len() > MAX_PLAYERS {
        return Err(DecodeError("too many players"));
    }
    p.players.into_iter().map(player_from_proto).collect()
}

fn state_to_proto(s: &StateMsg) -> proto::GameState {
    proto::GameState {
        state_order: s.order,
        snakes: s.snakes.iter().map(snake_to_proto).collect(),
        foods: s.foods.iter().map(|&f| coord_to_proto(f)).collect(),
        players: players_to_proto(&s.players),
        next_player_id: s.next_player_id,
    }
}

fn state_from_proto(s: proto::GameState) -> Result<StateMsg, DecodeError> {
    if s.snakes.len() > 64 {
        return Err(DecodeError("too many snakes"));
    }
    if s.foods.len() > 256 {
        return Err(DecodeError("too many foods"));
    }
    Ok(StateMsg {
        order: s.state_order,
        snakes: s
            .snakes
            .into_iter()
            .map(snake_from_proto)
            .collect::<Result<_, _>>()?,
        foods: s.foods.into_iter().map(coord_from_proto).collect(),
        players: players_from_proto(s.players)?,
        next_player_id: s.next_player_id,
    })
}

fn announcement_to_proto(a: &Announcement) -> proto::GameAnnouncement {
    proto::GameAnnouncement {
        players: players_to_proto(&a.players),
        config: config_to_proto(&a.config),
        can_join: Some(a.can_join),
        game_name: a.game_name.clone(),
    }
}

fn announcement_from_proto(a: proto::GameAnnouncement) -> Result<Announcement, DecodeError> {
    Ok(Announcement {
        players: players_from_proto(a.players)?,
        config: config_from_proto(a.config),
        can_join: a.can_join.unwrap_or(true),
        game_name: a.game_name,
    })
}

// ──────────────────────────── Проверка значений ──────────────────────────────

pub fn valid_player_name(s: &str) -> bool {
    !s.trim().is_empty() && s.len() <= MAX_PLAYER_NAME && !s.chars().any(char::is_control)
}

pub fn valid_game_name(s: &str) -> bool {
    !s.trim().is_empty() && s.len() <= MAX_GAME_NAME && !s.chars().any(char::is_control)
}

/// Обрезает по лимиту байт, отступая к границе UTF-8, а не разрывая символ.
pub fn truncate_bytes(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_owned()
}
