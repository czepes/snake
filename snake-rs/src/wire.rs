//! Сообщения протокола v2: типы и ручной кодек protobuf (proto2).
//!
//! Кодек написан вручную, чтобы не зависеть от `protoc` и точно управлять
//! строгостью разбора: обязательные поля проверяются, неизвестные поля
//! пропускаются (и никогда не копируются в исходящие сообщения, потому что
//! исходящие сообщения собираются из наших собственных типов).

use std::net::{Ipv4Addr, SocketAddrV4};
use std::str::FromStr;

pub const PROTOCOL_VERSION: u64 = 2;
/// Максимальный размер одной датаграммы протокола.
pub const MAX_DATAGRAM: usize = 1200;
pub const MAX_PLAYER_NAME: usize = 24;
pub const MAX_GAME_NAME: usize = 32;
pub const MAX_ERROR_TEXT: usize = 128;
pub const MAX_PLAYERS: usize = 6;
pub const MAX_SNAKES: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeRole {
    Normal,
    Master,
    Deputy,
}

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

    /// Единичный сдвиг: x вправо, y вниз.
    pub fn delta(self) -> (i32, i32) {
        match self {
            Dir::Up => (0, -1),
            Dir::Down => (0, 1),
            Dir::Left => (-1, 0),
            Dir::Right => (1, 0),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PlayerInfo {
    pub name: String,
    pub id: i32,
    /// Адрес участника. У описания отправителя сообщения отсутствует.
    pub addr: Option<SocketAddrV4>,
    pub role: NodeRole,
    pub ptype: PlayerType,
    pub score: i32,
    pub status: PlayerStatus,
}

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
    pub fn is_valid(&self) -> bool {
        (10..=100).contains(&self.width)
            && (10..=100).contains(&self.height)
            && (0..=10).contains(&self.food_static)
            && (100..=3000).contains(&self.state_delay_ms)
    }
}

/// Змейка в проводном виде: ключевые точки и направление головы.
#[derive(Clone, Debug, PartialEq)]
pub struct WireSnake {
    pub player_id: i32,
    pub points: Vec<(i32, i32)>,
    pub dir: Dir,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StateMsg {
    pub order: i32,
    pub snakes: Vec<WireSnake>,
    pub foods: Vec<(i32, i32)>,
    pub players: Vec<PlayerInfo>,
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

    /// Требует ли сообщение подтверждения ACK (всё, кроме Announcement, Discover и самого ACK).
    pub fn needs_ack(&self) -> bool {
        !matches!(self, Body::Announcement(_) | Body::Discover | Body::Ack)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Message {
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
}

// ─────────────────────────────── Запись ──────────────────────────────────────

fn put_varint(buf: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        buf.push((v as u8) | 0x80);
        v >>= 7;
    }
    buf.push(v as u8);
}

fn put_key(buf: &mut Vec<u8>, field: u32, wire_type: u8) {
    put_varint(buf, (u64::from(field) << 3) | u64::from(wire_type));
}

fn put_uint(buf: &mut Vec<u8>, field: u32, v: u64) {
    put_key(buf, field, 0);
    put_varint(buf, v);
}

/// int32 с отрицательным значением кодируется как 10-байтовый varint со знаковым расширением.
fn put_i32(buf: &mut Vec<u8>, field: u32, v: i32) {
    put_uint(buf, field, i64::from(v) as u64);
}

fn put_i64(buf: &mut Vec<u8>, field: u32, v: i64) {
    put_uint(buf, field, v as u64);
}

fn put_sint32(buf: &mut Vec<u8>, field: u32, v: i32) {
    put_uint(buf, field, u64::from(((v << 1) ^ (v >> 31)) as u32));
}

fn put_bytes(buf: &mut Vec<u8>, field: u32, data: &[u8]) {
    put_key(buf, field, 2);
    put_varint(buf, data.len() as u64);
    buf.extend_from_slice(data);
}

fn put_coord(buf: &mut Vec<u8>, field: u32, (x, y): (i32, i32)) {
    let mut inner = Vec::with_capacity(6);
    // Нулевая компонента не записывается: отсутствие поля означает 0.
    if x != 0 {
        put_sint32(&mut inner, 1, x);
    }
    if y != 0 {
        put_sint32(&mut inner, 2, y);
    }
    put_bytes(buf, field, &inner);
}

fn role_num(r: NodeRole) -> u64 {
    match r {
        NodeRole::Normal => 0,
        NodeRole::Master => 1,
        NodeRole::Deputy => 2,
    }
}

fn status_num(s: PlayerStatus) -> u64 {
    match s {
        PlayerStatus::Playing => 0,
        PlayerStatus::Viewing => 1,
    }
}

fn ptype_num(t: PlayerType) -> u64 {
    match t {
        PlayerType::Human => 0,
        PlayerType::Robot => 1,
    }
}

fn dir_num(d: Dir) -> u64 {
    match d {
        Dir::Up => 1,
        Dir::Down => 2,
        Dir::Left => 3,
        Dir::Right => 4,
    }
}

fn enc_player(p: &PlayerInfo) -> Vec<u8> {
    let mut b = Vec::with_capacity(48);
    put_bytes(&mut b, 1, p.name.as_bytes());
    put_i32(&mut b, 2, p.id);
    if let Some(a) = p.addr {
        put_bytes(&mut b, 3, a.ip().to_string().as_bytes());
        put_i32(&mut b, 4, i32::from(a.port()));
    }
    put_uint(&mut b, 5, role_num(p.role));
    put_uint(&mut b, 6, ptype_num(p.ptype));
    put_i32(&mut b, 7, p.score);
    put_uint(&mut b, 8, status_num(p.status));
    b
}

fn enc_players(players: &[PlayerInfo]) -> Vec<u8> {
    let mut b = Vec::new();
    for p in players {
        put_bytes(&mut b, 1, &enc_player(p));
    }
    b
}

fn enc_config(c: &Config) -> Vec<u8> {
    let mut b = Vec::with_capacity(12);
    put_i32(&mut b, 1, c.width);
    put_i32(&mut b, 2, c.height);
    put_i32(&mut b, 3, c.food_static);
    put_i32(&mut b, 5, c.state_delay_ms);
    b
}

fn enc_state(s: &StateMsg) -> Vec<u8> {
    let mut b = Vec::with_capacity(256);
    put_i32(&mut b, 1, s.order);
    for sn in &s.snakes {
        let mut sb = Vec::with_capacity(32);
        put_i32(&mut sb, 1, sn.player_id);
        for &pt in &sn.points {
            put_coord(&mut sb, 2, pt);
        }
        put_uint(&mut sb, 4, dir_num(sn.dir));
        put_bytes(&mut b, 2, &sb);
    }
    for &f in &s.foods {
        put_coord(&mut b, 3, f);
    }
    put_bytes(&mut b, 4, &enc_players(&s.players));
    put_i32(&mut b, 5, s.next_player_id);
    b
}

fn enc_announcement(a: &Announcement) -> Vec<u8> {
    let mut b = Vec::with_capacity(128);
    put_bytes(&mut b, 1, &enc_players(&a.players));
    put_bytes(&mut b, 2, &enc_config(&a.config));
    put_uint(&mut b, 3, u64::from(a.can_join));
    put_bytes(&mut b, 4, a.game_name.as_bytes());
    b
}

pub fn encode(m: &Message) -> Result<Vec<u8>, EncodeError> {
    let mut b = Vec::with_capacity(128);
    put_i64(&mut b, 1, m.seq);
    match &m.body {
        Body::Ping => put_bytes(&mut b, 2, &[]),
        Body::Steer(d) => {
            let mut x = Vec::new();
            put_uint(&mut x, 1, dir_num(*d));
            put_bytes(&mut b, 3, &x);
        }
        Body::Ack => put_bytes(&mut b, 4, &[]),
        Body::State(s) => {
            let mut x = Vec::new();
            put_bytes(&mut x, 1, &enc_state(s));
            put_bytes(&mut b, 5, &x);
        }
        Body::Announcement(games) => {
            let mut x = Vec::new();
            for g in games {
                put_bytes(&mut x, 1, &enc_announcement(g));
            }
            put_bytes(&mut b, 6, &x);
        }
        Body::Join {
            ptype,
            player_name,
            game_name,
            status,
        } => {
            let mut x = Vec::new();
            put_uint(&mut x, 1, ptype_num(*ptype));
            put_bytes(&mut x, 3, player_name.as_bytes());
            put_bytes(&mut x, 4, game_name.as_bytes());
            put_uint(&mut x, 6, status_num(*status));
            put_bytes(&mut b, 7, &x);
        }
        Body::Error { text, request_seq } => {
            let mut x = Vec::new();
            put_bytes(&mut x, 1, text.as_bytes());
            put_i64(&mut x, 2, *request_seq);
            put_bytes(&mut b, 8, &x);
        }
        Body::RoleChange {
            sender,
            receiver,
            order,
        } => {
            let mut x = Vec::new();
            if let Some(r) = sender {
                put_uint(&mut x, 1, role_num(*r));
            }
            if let Some(r) = receiver {
                put_uint(&mut x, 2, role_num(*r));
            }
            put_i32(&mut x, 3, *order);
            put_bytes(&mut b, 9, &x);
        }
        Body::Discover => put_bytes(&mut b, 12, &[]),
        Body::StatusChange(s) => {
            let mut x = Vec::new();
            put_uint(&mut x, 1, status_num(*s));
            put_bytes(&mut b, 15, &x);
        }
        Body::Leave => put_bytes(&mut b, 16, &[]),
    }
    if let Some(id) = m.sender_id {
        put_i32(&mut b, 10, id);
    }
    if let Some(id) = m.receiver_id {
        put_i32(&mut b, 11, id);
    }
    put_uint(&mut b, 13, PROTOCOL_VERSION);
    if let Some(g) = &m.game_name {
        put_bytes(&mut b, 17, g.as_bytes());
    }
    if b.len() > MAX_DATAGRAM {
        return Err(EncodeError::TooLarge(b.len()));
    }
    Ok(b)
}

// ─────────────────────────────── Чтение ──────────────────────────────────────

struct Rd<'a> {
    b: &'a [u8],
    p: usize,
}

type R<T> = Result<T, DecodeError>;

impl<'a> Rd<'a> {
    fn new(b: &'a [u8]) -> Self {
        Rd { b, p: 0 }
    }

    fn eof(&self) -> bool {
        self.p >= self.b.len()
    }

    fn varint(&mut self) -> R<u64> {
        let mut v: u64 = 0;
        for shift in (0..70).step_by(7) {
            let byte = *self.b.get(self.p).ok_or(DecodeError("truncated varint"))?;
            self.p += 1;
            if shift == 63 && byte > 1 {
                return Err(DecodeError("varint overflow"));
            }
            v |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err(DecodeError("varint too long"))
    }

    fn key(&mut self) -> R<(u32, u8)> {
        let k = self.varint()?;
        let field = k >> 3;
        if field == 0 || field > u64::from(u32::MAX >> 3) {
            return Err(DecodeError("bad field number"));
        }
        Ok((field as u32, (k & 7) as u8))
    }

    fn bytes(&mut self) -> R<&'a [u8]> {
        let len = usize::try_from(self.varint()?).map_err(|_| DecodeError("bad length"))?;
        let end = self.p.checked_add(len).ok_or(DecodeError("bad length"))?;
        let s = self.b.get(self.p..end).ok_or(DecodeError("truncated field"))?;
        self.p = end;
        Ok(s)
    }

    fn skip(&mut self, wire_type: u8) -> R<()> {
        match wire_type {
            0 => {
                self.varint()?;
            }
            1 => self.advance(8)?,
            2 => {
                self.bytes()?;
            }
            5 => self.advance(4)?,
            _ => return Err(DecodeError("unsupported wire type")),
        }
        Ok(())
    }

    fn advance(&mut self, n: usize) -> R<()> {
        if self.p + n > self.b.len() {
            return Err(DecodeError("truncated field"));
        }
        self.p += n;
        Ok(())
    }
}

fn want(wire_type: u8, expected: u8) -> R<()> {
    if wire_type == expected {
        Ok(())
    } else {
        Err(DecodeError("wrong wire type"))
    }
}

fn as_i32(v: u64) -> i32 {
    v as i64 as i32
}

fn zigzag32(v: u64) -> i32 {
    let v = v as u32;
    ((v >> 1) as i32) ^ -((v & 1) as i32)
}

fn text(b: &[u8]) -> R<String> {
    std::str::from_utf8(b)
        .map(str::to_owned)
        .map_err(|_| DecodeError("invalid utf-8"))
}

fn role_from(v: u64) -> Option<NodeRole> {
    match v {
        0 => Some(NodeRole::Normal),
        1 => Some(NodeRole::Master),
        2 => Some(NodeRole::Deputy),
        _ => None,
    }
}

fn status_from(v: u64) -> Option<PlayerStatus> {
    match v {
        0 => Some(PlayerStatus::Playing),
        1 => Some(PlayerStatus::Viewing),
        _ => None,
    }
}

fn ptype_from(v: u64) -> Option<PlayerType> {
    match v {
        0 => Some(PlayerType::Human),
        1 => Some(PlayerType::Robot),
        _ => None,
    }
}

fn dir_from(v: u64) -> Option<Dir> {
    match v {
        1 => Some(Dir::Up),
        2 => Some(Dir::Down),
        3 => Some(Dir::Left),
        4 => Some(Dir::Right),
        _ => None,
    }
}

fn dec_coord(b: &[u8]) -> R<(i32, i32)> {
    let mut r = Rd::new(b);
    let (mut x, mut y) = (0, 0);
    while !r.eof() {
        let (f, wt) = r.key()?;
        match f {
            1 => {
                want(wt, 0)?;
                x = zigzag32(r.varint()?);
            }
            2 => {
                want(wt, 0)?;
                y = zigzag32(r.varint()?);
            }
            _ => r.skip(wt)?,
        }
    }
    Ok((x, y))
}

fn dec_player(b: &[u8]) -> R<PlayerInfo> {
    let mut r = Rd::new(b);
    let (mut name, mut id, mut ip, mut port) = (None, None, None, None);
    let (mut role, mut ptype, mut score, mut status) = (None, PlayerType::Human, None, None);
    while !r.eof() {
        let (f, wt) = r.key()?;
        match f {
            1 => {
                want(wt, 2)?;
                name = Some(text(r.bytes()?)?);
            }
            2 => {
                want(wt, 0)?;
                id = Some(as_i32(r.varint()?));
            }
            3 => {
                want(wt, 2)?;
                ip = Some(text(r.bytes()?)?);
            }
            4 => {
                want(wt, 0)?;
                port = Some(as_i32(r.varint()?));
            }
            5 => {
                want(wt, 0)?;
                role = Some(role_from(r.varint()?).ok_or(DecodeError("bad role"))?);
            }
            6 => {
                want(wt, 0)?;
                // Неизвестное значение необязательного enum считается отсутствующим.
                if let Some(t) = ptype_from(r.varint()?) {
                    ptype = t;
                }
            }
            7 => {
                want(wt, 0)?;
                score = Some(as_i32(r.varint()?));
            }
            8 => {
                want(wt, 0)?;
                status = Some(status_from(r.varint()?).ok_or(DecodeError("bad status"))?);
            }
            _ => r.skip(wt)?,
        }
    }
    let addr = match (ip, port) {
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
        name: name.ok_or(DecodeError("player without name"))?,
        id: id.ok_or(DecodeError("player without id"))?,
        addr,
        role: role.ok_or(DecodeError("player without role"))?,
        ptype,
        score: score.ok_or(DecodeError("player without score"))?,
        status: status.ok_or(DecodeError("player without status"))?,
    })
}

fn dec_players(b: &[u8]) -> R<Vec<PlayerInfo>> {
    let mut r = Rd::new(b);
    let mut out = Vec::new();
    while !r.eof() {
        let (f, wt) = r.key()?;
        if f == 1 {
            want(wt, 2)?;
            out.push(dec_player(r.bytes()?)?);
            if out.len() > 64 {
                return Err(DecodeError("too many players"));
            }
        } else {
            r.skip(wt)?;
        }
    }
    Ok(out)
}

fn dec_config(b: &[u8]) -> R<Config> {
    let mut r = Rd::new(b);
    let mut c = Config::default();
    while !r.eof() {
        let (f, wt) = r.key()?;
        match f {
            1 | 2 | 3 | 5 => {
                want(wt, 0)?;
                let v = as_i32(r.varint()?);
                match f {
                    1 => c.width = v,
                    2 => c.height = v,
                    3 => c.food_static = v,
                    _ => c.state_delay_ms = v,
                }
            }
            _ => r.skip(wt)?,
        }
    }
    Ok(c)
}

fn dec_snake(b: &[u8]) -> R<WireSnake> {
    let mut r = Rd::new(b);
    let (mut id, mut dir) = (None, None);
    let mut points = Vec::new();
    while !r.eof() {
        let (f, wt) = r.key()?;
        match f {
            1 => {
                want(wt, 0)?;
                id = Some(as_i32(r.varint()?));
            }
            2 => {
                want(wt, 2)?;
                points.push(dec_coord(r.bytes()?)?);
                if points.len() > 2048 {
                    return Err(DecodeError("too many points"));
                }
            }
            4 => {
                want(wt, 0)?;
                dir = Some(dir_from(r.varint()?).ok_or(DecodeError("bad direction"))?);
            }
            _ => r.skip(wt)?,
        }
    }
    Ok(WireSnake {
        player_id: id.ok_or(DecodeError("snake without id"))?,
        points,
        dir: dir.ok_or(DecodeError("snake without direction"))?,
    })
}

fn dec_state(b: &[u8]) -> R<StateMsg> {
    let mut r = Rd::new(b);
    let (mut order, mut players, mut next_id) = (None, None, None);
    let (mut snakes, mut foods) = (Vec::new(), Vec::new());
    while !r.eof() {
        let (f, wt) = r.key()?;
        match f {
            1 => {
                want(wt, 0)?;
                order = Some(as_i32(r.varint()?));
            }
            2 => {
                want(wt, 2)?;
                snakes.push(dec_snake(r.bytes()?)?);
                if snakes.len() > 64 {
                    return Err(DecodeError("too many snakes"));
                }
            }
            3 => {
                want(wt, 2)?;
                foods.push(dec_coord(r.bytes()?)?);
                if foods.len() > 256 {
                    return Err(DecodeError("too many foods"));
                }
            }
            4 => {
                want(wt, 2)?;
                players = Some(dec_players(r.bytes()?)?);
            }
            5 => {
                want(wt, 0)?;
                next_id = Some(as_i32(r.varint()?));
            }
            _ => r.skip(wt)?,
        }
    }
    Ok(StateMsg {
        order: order.ok_or(DecodeError("state without order"))?,
        snakes,
        foods,
        players: players.ok_or(DecodeError("state without players"))?,
        next_player_id: next_id.ok_or(DecodeError("state without next_player_id"))?,
    })
}

fn dec_announcement(b: &[u8]) -> R<Announcement> {
    let mut r = Rd::new(b);
    let (mut players, mut config, mut name) = (None, None, None);
    let mut can_join = true;
    while !r.eof() {
        let (f, wt) = r.key()?;
        match f {
            1 => {
                want(wt, 2)?;
                players = Some(dec_players(r.bytes()?)?);
            }
            2 => {
                want(wt, 2)?;
                config = Some(dec_config(r.bytes()?)?);
            }
            3 => {
                want(wt, 0)?;
                can_join = r.varint()? != 0;
            }
            4 => {
                want(wt, 2)?;
                name = Some(text(r.bytes()?)?);
            }
            _ => r.skip(wt)?,
        }
    }
    Ok(Announcement {
        players: players.ok_or(DecodeError("announcement without players"))?,
        config: config.ok_or(DecodeError("announcement without config"))?,
        can_join,
        game_name: name.ok_or(DecodeError("announcement without game_name"))?,
    })
}

fn dec_join(b: &[u8]) -> R<Body> {
    let mut r = Rd::new(b);
    let mut ptype = PlayerType::Human;
    let (mut pname, mut gname, mut status) = (None, None, None);
    while !r.eof() {
        let (f, wt) = r.key()?;
        match f {
            1 => {
                want(wt, 0)?;
                if let Some(t) = ptype_from(r.varint()?) {
                    ptype = t;
                }
            }
            3 => {
                want(wt, 2)?;
                pname = Some(text(r.bytes()?)?);
            }
            4 => {
                want(wt, 2)?;
                gname = Some(text(r.bytes()?)?);
            }
            6 => {
                want(wt, 0)?;
                status = Some(status_from(r.varint()?).ok_or(DecodeError("bad status"))?);
            }
            _ => r.skip(wt)?,
        }
    }
    Ok(Body::Join {
        ptype,
        player_name: pname.ok_or(DecodeError("join without player_name"))?,
        game_name: gname.ok_or(DecodeError("join without game_name"))?,
        status: status.ok_or(DecodeError("join without status"))?,
    })
}

fn dec_error(b: &[u8]) -> R<Body> {
    let mut r = Rd::new(b);
    let (mut t, mut seq) = (None, None);
    while !r.eof() {
        let (f, wt) = r.key()?;
        match f {
            1 => {
                want(wt, 2)?;
                t = Some(text(r.bytes()?)?);
            }
            2 => {
                want(wt, 0)?;
                seq = Some(r.varint()? as i64);
            }
            _ => r.skip(wt)?,
        }
    }
    Ok(Body::Error {
        text: t.ok_or(DecodeError("error without text"))?,
        request_seq: seq.ok_or(DecodeError("error without request_seq"))?,
    })
}

fn dec_role_change(b: &[u8]) -> R<Body> {
    let mut r = Rd::new(b);
    let (mut sender, mut receiver, mut order) = (None, None, None);
    while !r.eof() {
        let (f, wt) = r.key()?;
        match f {
            1 => {
                want(wt, 0)?;
                sender = role_from(r.varint()?);
            }
            2 => {
                want(wt, 0)?;
                receiver = role_from(r.varint()?);
            }
            3 => {
                want(wt, 0)?;
                order = Some(as_i32(r.varint()?));
            }
            _ => r.skip(wt)?,
        }
    }
    Ok(Body::RoleChange {
        sender,
        receiver,
        order: order.ok_or(DecodeError("role change without order"))?,
    })
}

/// Достаёт единственное обязательное поле-перечисление из вложенного сообщения.
fn dec_single_enum(b: &[u8], field: u32) -> R<u64> {
    let mut r = Rd::new(b);
    let mut v = None;
    while !r.eof() {
        let (f, wt) = r.key()?;
        if f == field {
            want(wt, 0)?;
            v = Some(r.varint()?);
        } else {
            r.skip(wt)?;
        }
    }
    v.ok_or(DecodeError("missing required field"))
}

pub fn decode(bytes: &[u8]) -> Result<Message, DecodeError> {
    let mut r = Rd::new(bytes);
    let (mut seq, mut version) = (None, None);
    let (mut sender_id, mut receiver_id, mut game_name) = (None, None, None);
    let mut body: Option<Body> = None;
    while !r.eof() {
        let (f, wt) = r.key()?;
        match f {
            1 => {
                want(wt, 0)?;
                seq = Some(r.varint()? as i64);
            }
            10 => {
                want(wt, 0)?;
                sender_id = Some(as_i32(r.varint()?));
            }
            11 => {
                want(wt, 0)?;
                receiver_id = Some(as_i32(r.varint()?));
            }
            13 => {
                want(wt, 0)?;
                version = Some(r.varint()?);
            }
            17 => {
                want(wt, 2)?;
                game_name = Some(text(r.bytes()?)?);
            }
            2 => {
                want(wt, 2)?;
                r.bytes()?;
                body = Some(Body::Ping);
            }
            3 => {
                want(wt, 2)?;
                let d = dir_from(dec_single_enum(r.bytes()?, 1)?).ok_or(DecodeError("bad direction"))?;
                body = Some(Body::Steer(d));
            }
            4 => {
                want(wt, 2)?;
                r.bytes()?;
                body = Some(Body::Ack);
            }
            5 => {
                want(wt, 2)?;
                let mut inner = Rd::new(r.bytes()?);
                let mut state = None;
                while !inner.eof() {
                    let (f2, wt2) = inner.key()?;
                    if f2 == 1 {
                        want(wt2, 2)?;
                        state = Some(dec_state(inner.bytes()?)?);
                    } else {
                        inner.skip(wt2)?;
                    }
                }
                body = Some(Body::State(state.ok_or(DecodeError("state message without state"))?));
            }
            6 => {
                want(wt, 2)?;
                let mut inner = Rd::new(r.bytes()?);
                let mut games = Vec::new();
                while !inner.eof() {
                    let (f2, wt2) = inner.key()?;
                    if f2 == 1 {
                        want(wt2, 2)?;
                        games.push(dec_announcement(inner.bytes()?)?);
                        if games.len() > 64 {
                            return Err(DecodeError("too many games"));
                        }
                    } else {
                        inner.skip(wt2)?;
                    }
                }
                body = Some(Body::Announcement(games));
            }
            7 => {
                want(wt, 2)?;
                body = Some(dec_join(r.bytes()?)?);
            }
            8 => {
                want(wt, 2)?;
                body = Some(dec_error(r.bytes()?)?);
            }
            9 => {
                want(wt, 2)?;
                body = Some(dec_role_change(r.bytes()?)?);
            }
            12 => {
                want(wt, 2)?;
                r.bytes()?;
                body = Some(Body::Discover);
            }
            15 => {
                want(wt, 2)?;
                let s = status_from(dec_single_enum(r.bytes()?, 1)?).ok_or(DecodeError("bad status"))?;
                body = Some(Body::StatusChange(s));
            }
            16 => {
                want(wt, 2)?;
                r.bytes()?;
                body = Some(Body::Leave);
            }
            _ => r.skip(wt)?,
        }
    }
    if version != Some(PROTOCOL_VERSION) {
        return Err(DecodeError("unsupported protocol version"));
    }
    let seq = seq.ok_or(DecodeError("missing msg_seq"))?;
    if seq < 0 {
        return Err(DecodeError("negative msg_seq"));
    }
    Ok(Message {
        seq,
        sender_id,
        receiver_id,
        game_name,
        body: body.ok_or(DecodeError("missing message type"))?,
    })
}

// ──────────────────────────── Проверка значений ──────────────────────────────

pub fn valid_player_name(s: &str) -> bool {
    !s.trim().is_empty() && s.len() <= MAX_PLAYER_NAME && !s.chars().any(char::is_control)
}

pub fn valid_game_name(s: &str) -> bool {
    !s.trim().is_empty() && s.len() <= MAX_GAME_NAME && !s.chars().any(char::is_control)
}

/// Обрезает текст до `max` байт по границе символа.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn player(id: i32, with_addr: bool) -> PlayerInfo {
        PlayerInfo {
            name: format!("игрок-{id}"),
            id,
            addr: with_addr.then(|| SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, id as u8), 40000 + id as u16)),
            role: if id == 1 { NodeRole::Master } else { NodeRole::Normal },
            ptype: PlayerType::Human,
            score: id * 7,
            status: PlayerStatus::Playing,
        }
    }

    fn roundtrip(body: Body) {
        let m = Message {
            seq: 123_456_789_012,
            sender_id: Some(3),
            receiver_id: Some(-1),
            game_name: Some("партия".into()),
            body,
        };
        let bytes = encode(&m).unwrap();
        assert_eq!(decode(&bytes).unwrap(), m);
    }

    #[test]
    fn roundtrips_every_message_type() {
        roundtrip(Body::Ping);
        roundtrip(Body::Steer(Dir::Left));
        roundtrip(Body::Ack);
        roundtrip(Body::Discover);
        roundtrip(Body::Leave);
        roundtrip(Body::StatusChange(PlayerStatus::Viewing));
        roundtrip(Body::Error {
            text: "нет мест".into(),
            request_seq: 77,
        });
        roundtrip(Body::RoleChange {
            sender: Some(NodeRole::Normal),
            receiver: Some(NodeRole::Master),
            order: 9,
        });
        roundtrip(Body::RoleChange {
            sender: None,
            receiver: Some(NodeRole::Deputy),
            order: 0,
        });
        roundtrip(Body::Join {
            ptype: PlayerType::Human,
            player_name: "Маша".into(),
            game_name: "игра".into(),
            status: PlayerStatus::Viewing,
        });
        roundtrip(Body::State(StateMsg {
            order: 5,
            snakes: vec![WireSnake {
                player_id: 2,
                points: vec![(5, 1), (3, 0), (0, 2), (-4, 0)],
                dir: Dir::Left,
            }],
            foods: vec![(0, 0), (10, -3)],
            players: vec![player(1, false), player(2, true)],
            next_player_id: 3,
        }));
        roundtrip(Body::Announcement(vec![Announcement {
            players: vec![player(1, false)],
            config: Config {
                width: 20,
                height: 15,
                food_static: 2,
                state_delay_ms: 300,
            },
            can_join: false,
            game_name: "Практика".into(),
        }]));
    }

    #[test]
    fn rejects_wrong_version_and_missing_fields() {
        let mut m = encode(&Message {
            seq: 1,
            sender_id: None,
            receiver_id: None,
            game_name: None,
            body: Body::Discover,
        })
        .unwrap();
        assert!(decode(&m).is_ok());
        // Подменяем версию: поле 13, значение 3.
        let pos = m.len() - 1;
        m[pos] = 3;
        assert_eq!(decode(&m), Err(DecodeError("unsupported protocol version")));
        assert!(decode(&[]).is_err());
        assert!(decode(&[0xff; 20]).is_err());
    }

    #[test]
    fn skips_unknown_fields() {
        let mut bytes = encode(&Message {
            seq: 4,
            sender_id: None,
            receiver_id: None,
            game_name: Some("g".into()),
            body: Body::Ping,
        })
        .unwrap();
        // Неизвестное поле 99 (varint) и поле 98 (длинное).
        put_uint(&mut bytes, 99, 12345);
        put_bytes(&mut bytes, 98, b"junk");
        let m = decode(&bytes).unwrap();
        assert_eq!(m.body, Body::Ping);
    }

    #[test]
    fn encoder_never_exceeds_datagram_limit() {
        let players: Vec<_> = (1..=6).map(|i| player(i, i != 1)).collect();
        let snakes: Vec<_> = (1..=4)
            .map(|i| WireSnake {
                player_id: i,
                points: vec![(99, 99), (-31, 0), (0, -31), (31, 0)],
                dir: Dir::Up,
            })
            .collect();
        let state = StateMsg {
            order: i32::MAX,
            snakes,
            foods: vec![(99, 99); 14],
            players,
            next_player_id: i32::MAX,
        };
        let bytes = encode(&Message {
            seq: i64::MAX,
            sender_id: Some(i32::MAX),
            receiver_id: Some(i32::MAX),
            game_name: Some("я".repeat(16)),
            body: Body::State(state),
        })
        .unwrap();
        assert!(bytes.len() <= MAX_DATAGRAM, "len = {}", bytes.len());
    }
}
