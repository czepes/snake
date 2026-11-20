//! Central session engine: single-threaded event loop driving discovery,
//! join/host flow, MASTER simulation + broadcast, and DEPUTY/NORMAL failover,
//! per the state machine in `snakes_task.md`.

use std::collections::{BTreeMap, HashMap};
use std::net::{SocketAddr, SocketAddrV4};
use std::time::{Duration, Instant};

use crate::discovery::{Catalog, GameId};
use crate::game::{Game, JoinError, NodeRole, Player, PlayerStatus};
use crate::model::{
    Direction, GameConfig, ANNOUNCE_INTERVAL, ANNOUNCE_TTL, DISCOVER_INTERVAL_BROWSING,
    DISCOVER_INTERVAL_WITH_CONFIG, JOIN_TOTAL_TIMEOUT, MAX_ERROR_BYTES, MAX_GAME_NAME_BYTES,
    MAX_PLAYER_NAME_BYTES, MAX_PLAYERS, PEER_TIMEOUT, PING_INTERVAL,
    RESEND_INTERVAL, truncate_utf8,
};
use crate::net;
use crate::proto::{self, GameMessage};
use crate::transport::{self, PeerState};

/// Configuration for the fixed, course-provided Internet discovery server.
pub const COURSE_SERVER: &str = "snakes.ippolitov.me:9193";

pub enum UiEvent {
    /// User picked a game to join/watch from the discovery list.
    Join {
        id: GameId,
        player_name: String,
        as_player: bool,
    },
    /// User wants to host a brand-new local game.
    Host {
        game_name: String,
        player_name: String,
        config: GameConfig,
    },
    Steer(Direction),
    /// "Смотреть / прекратить играть".
    ToggleStatus,
    /// Full "Выйти из игры".
    Leave,
    Quit,
}

/// What the local user currently sees: the discovery browser, or an active
/// game session (as participant, with some network role).
pub enum Mode {
    Browsing,
    InGame,
}

/// Per-connection bookkeeping for a participant other than ourselves.
struct Remote {
    id: i32,
    addr: SocketAddrV4,
    peer: PeerState,
    next_seq: i64,
}

impl Remote {
    fn new(id: i32, addr: SocketAddrV4) -> Self {
        Self {
            id,
            addr,
            peer: PeerState::new(addr),
            next_seq: 0,
        }
    }

    fn alloc_seq(&mut self) -> i64 {
        let s = self.next_seq;
        self.next_seq += 1;
        s
    }
}

/// Bootstrap state while waiting for a Join ACK + initial config/state.
struct JoiningState {
    game_name: String,
    master_addr: SocketAddrV4,
    player_name: String,
    as_player: bool,
    join_seq: i64,
    started: Instant,
    last_sent: Instant,
    acked: bool,
    discover_sent: Instant,
    assigned_id: Option<i32>,
    buffered_state: Option<(i32, proto::GameState)>,
}

pub struct Session {
    pub sockets_unicast: std::net::UdpSocket,
    pub sockets_mcast: std::net::UdpSocket,
    pub catalog: Catalog,
    pub last_discover_broadcast: Instant,
    pub course_server_addr: Option<SocketAddrV4>,

    pub mode: Mode,
    pub error: Option<String>,

    // Active game context (set once joined/hosted).
    pub game_name: Option<String>,
    pub config: Option<GameConfig>,
    pub my_id: Option<i32>,
    pub my_role: NodeRole,
    pub my_status: PlayerStatus,
    pub my_name: String,
    pub master_addr: Option<SocketAddrV4>,
    pub deputy_addr: Option<SocketAddrV4>,
    pub deputy_id: Option<i32>,

    /// Authoritative simulation; Some() only when we are MASTER or DEPUTY.
    pub game: Option<Game>,
    /// Last rendered snapshot (used directly by NORMAL, kept in sync by
    /// MASTER/DEPUTY too for display code reuse).
    pub last_state: Option<proto::GameState>,
    pub last_state_order: i32,
    last_tick: Instant,

    remotes: HashMap<i32, Remote>,
    joining: Option<JoiningState>,
    my_seq: i64,
    pending_steer: Option<Direction>,
    requested_moves: BTreeMap<i32, Direction>,

    pub should_quit: bool,
}

impl Session {
    pub fn new() -> std::io::Result<Self> {
        let unicast = net::bind_unicast()?;
        let mcast = net::bind_multicast_receiver()?;
        let course_server_addr = resolve_course_server();
        Ok(Self {
            sockets_unicast: unicast,
            sockets_mcast: mcast,
            catalog: Catalog::new(),
            last_discover_broadcast: Instant::now() - DISCOVER_INTERVAL_BROWSING,
            course_server_addr,
            mode: Mode::Browsing,
            error: None,
            game_name: None,
            config: None,
            my_id: None,
            my_role: NodeRole::Normal,
            my_status: PlayerStatus::Viewing,
            my_name: String::new(),
            master_addr: None,
            deputy_addr: None,
            deputy_id: None,
            game: None,
            last_state: None,
            last_state_order: -1,
            last_tick: Instant::now(),
            remotes: HashMap::new(),
            joining: None,
            my_seq: 0,
            pending_steer: None,
            requested_moves: BTreeMap::new(),
            should_quit: false,
        })
    }

    fn alloc_my_seq(&mut self) -> i64 {
        let s = self.my_seq;
        self.my_seq += 1;
        s
    }

    pub fn is_master(&self) -> bool {
        matches!(self.my_role, NodeRole::Master)
    }

    pub fn is_deputy(&self) -> bool {
        matches!(self.my_role, NodeRole::Deputy)
    }

    // ---------------------------------------------------------------
    // Top-level driving loop
    // ---------------------------------------------------------------

    pub fn handle_ui_event(&mut self, ev: UiEvent) {
        match ev {
            UiEvent::Quit => {
                self.leave_if_in_game();
                self.should_quit = true;
            }
            UiEvent::Host {
                game_name,
                player_name,
                config,
            } => self.host_game(game_name, player_name, config),
            UiEvent::Join {
                id,
                player_name,
                as_player,
            } => self.start_join(id, player_name, as_player),
            UiEvent::Steer(d) => self.pending_steer = Some(d),
            UiEvent::ToggleStatus => self.request_toggle_status(),
            UiEvent::Leave => self.leave_if_in_game(),
        }
    }

    pub fn tick(&mut self) {
        self.poll_sockets();
        self.drive_discovery();
        self.drive_join_bootstrap();
        self.drive_ping_and_timeouts();
        self.drive_resends();
        if self.is_master() {
            self.drive_master_tick();
        }
        self.catalog.expire(ANNOUNCE_TTL);
    }

    fn leave_if_in_game(&mut self) {
        if self.game_name.is_none() {
            return;
        }
        if let (Some(master), Some(id)) = (self.master_addr, self.my_id) {
            if !self.is_master() {
                let seq = self.alloc_my_seq();
                let mut m = transport::make_envelope(
                    seq,
                    Some(id),
                    None,
                    self.game_name.as_deref(),
                );
                m.set_leave(proto::game_message::LeaveMsg::new());
                self.send_best_effort(&m, master);
            }
        }
        self.reset_game_state();
    }

    fn reset_game_state(&mut self) {
        self.mode = Mode::Browsing;
        self.game_name = None;
        self.config = None;
        self.my_id = None;
        self.my_role = NodeRole::Normal;
        self.my_status = PlayerStatus::Viewing;
        self.master_addr = None;
        self.deputy_addr = None;
        self.deputy_id = None;
        self.game = None;
        self.last_state = None;
        self.last_state_order = -1;
        self.remotes.clear();
        self.joining = None;
        self.my_seq = 0;
        self.pending_steer = None;
        self.requested_moves.clear();
    }

    // ---------------------------------------------------------------
    // Hosting
    // ---------------------------------------------------------------

    fn host_game(&mut self, game_name: String, player_name: String, config: GameConfig) {
        if !config.is_valid() {
            self.error = Some("Некорректные параметры игры".to_string());
            return;
        }
        self.reset_game_state();
        let game_name = truncate_utf8(&game_name, MAX_GAME_NAME_BYTES);
        let player_name = truncate_utf8(&player_name, MAX_PLAYER_NAME_BYTES);
        let mut game = Game::new(config);
        let id = game.alloc_player_id().unwrap();
        if game.join_player(id).is_err() {
            self.error = Some("Не удалось разместить змейку".to_string());
            return;
        }
        game.players.insert(
            id,
            Player {
                id,
                name: player_name.clone(),
                addr: None,
                role: NodeRole::Master,
                score: 0,
                status: PlayerStatus::Playing,
            },
        );
        game.state_order = 0;
        self.game = Some(game);
        self.game_name = Some(game_name);
        self.config = Some(config);
        self.my_id = Some(id);
        self.my_name = player_name;
        self.my_role = NodeRole::Master;
        self.my_status = PlayerStatus::Playing;
        self.mode = Mode::InGame;
        self.publish_state();
    }

    // ---------------------------------------------------------------
    // Joining
    // ---------------------------------------------------------------

    fn start_join(&mut self, id: GameId, player_name: String, as_player: bool) {
        self.reset_game_state();
        let player_name = truncate_utf8(&player_name, MAX_PLAYER_NAME_BYTES);
        let seq = self.alloc_my_seq();
        self.joining = Some(JoiningState {
            game_name: id.game_name.clone(),
            master_addr: id.master_addr,
            player_name: player_name.clone(),
            as_player,
            join_seq: seq,
            started: Instant::now(),
            last_sent: Instant::now() - Duration::from_secs(1),
            acked: false,
            discover_sent: Instant::now() - Duration::from_secs(1),
            assigned_id: None,
            buffered_state: None,
        });
        self.my_name = player_name;
        self.mode = Mode::InGame;
    }

    fn drive_join_bootstrap(&mut self) {
        if self.joining.is_none() {
            return;
        }
        let now = Instant::now();

        {
            let js = self.joining.as_ref().unwrap();
            if now.duration_since(js.started) > JOIN_TOTAL_TIMEOUT && js.buffered_state.is_none() {
                self.error = Some("Не удалось подключиться к игре: истекло время ожидания".into());
                self.joining = None;
                self.mode = Mode::Browsing;
                return;
            }
        }

        {
            let js = self.joining.as_ref().unwrap();
            if !js.acked && now.duration_since(js.last_sent) >= RESEND_INTERVAL {
                let mut m = transport::make_envelope(js.join_seq, None, None, Some(&js.game_name));
                let mut join = proto::game_message::JoinMsg::new();
                join.set_player_name(js.player_name.clone());
                join.set_game_name(js.game_name.clone());
                join.set_requested_status(if js.as_player {
                    proto::PlayerStatus::PLAYING
                } else {
                    proto::PlayerStatus::VIEWING
                });
                m.set_join(join);
                let master_addr = js.master_addr;
                if let Some(bytes) = transport::encode(&m) {
                    let _ = self.sockets_unicast.send_to(&bytes, SocketAddr::V4(master_addr));
                }
                self.joining.as_mut().unwrap().last_sent = now;
            }
        }

        {
            let needs_discover = {
                let js = self.joining.as_ref().unwrap();
                (js.assigned_id.is_none() || self.config.is_none())
                    && now.duration_since(js.discover_sent) >= DISCOVER_INTERVAL_WITH_CONFIG
            };
            if needs_discover {
                let master_addr = self.joining.as_ref().unwrap().master_addr;
                let seq = self.alloc_my_seq();
                let mut m = transport::make_envelope(seq, None, None, None);
                m.set_discover(proto::game_message::DiscoverMsg::new());
                if let Some(bytes) = transport::encode(&m) {
                    let _ = self.sockets_unicast.send_to(&bytes, SocketAddr::V4(master_addr));
                }
                self.joining.as_mut().unwrap().discover_sent = now;
            }
        }

        let (assigned_id, buffered_js) = {
            let js = self.joining.as_ref().unwrap();
            (js.assigned_id, js.assigned_id.is_some() && self.config.is_some())
        };
        let _ = buffered_js;

        if let Some(id) = assigned_id {
            if let Some(cfg) = self.config {
                // Fully bootstrapped: finalize.
                let js = self.joining.as_mut().unwrap();
                let game_name = js.game_name.clone();
                let master_addr = js.master_addr;
                let as_player = js.as_player;
                let buffered = js.buffered_state.take();
                self.my_id = Some(id);
                self.game_name = Some(game_name);
                self.master_addr = Some(master_addr);
                self.config = Some(cfg);
                self.my_role = NodeRole::Normal;
                self.my_status = if as_player {
                    PlayerStatus::Playing
                } else {
                    PlayerStatus::Viewing
                };
                self.joining = None;
                if let Some((order, state)) = buffered {
                    self.apply_state_snapshot(order, &state);
                }
            }
        }
    }

    // ---------------------------------------------------------------
    // Discovery
    // ---------------------------------------------------------------

    fn drive_discovery(&mut self) {
        if !matches!(self.mode, Mode::Browsing) {
            return;
        }
        let now = Instant::now();
        if now.duration_since(self.last_discover_broadcast) >= DISCOVER_INTERVAL_BROWSING {
            self.last_discover_broadcast = now;
            let mut m = transport::make_envelope(0, None, None, None);
            m.set_discover(proto::game_message::DiscoverMsg::new());
            if let Some(bytes) = transport::encode(&m) {
                if let Some(server) = self.course_server_addr {
                    let _ = self.sockets_unicast.send_to(&bytes, SocketAddr::V4(server));
                }
            }
        }
    }

    // ---------------------------------------------------------------
    // Socket polling
    // ---------------------------------------------------------------

    fn poll_sockets(&mut self) {
        let mut buf = [0u8; 2048];
        loop {
            match self.sockets_mcast.recv_from(&mut buf) {
                Ok((n, from)) => self.handle_datagram(&buf[..n], from, true),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
        loop {
            match self.sockets_unicast.recv_from(&mut buf) {
                Ok((n, from)) => self.handle_datagram(&buf[..n], from, false),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
    }

    fn handle_datagram(&mut self, bytes: &[u8], from: SocketAddr, is_multicast: bool) {
        let Some(addr) = transport::socket_addr_v4(from) else { return };
        let Some(msg) = transport::decode(bytes) else { return };

        if msg.has_discover() {
            if !is_multicast {
                self.reply_discover(addr);
            }
            return;
        }
        if msg.has_announcement() {
            for g in &msg.announcement().games {
                self.catalog.ingest(addr, g, !is_multicast && self.is_server_addr(addr));
            }
            // While bootstrapping a join, the config is only ever learned
            // from an Announcement of the chosen endpoint+game_name (see
            // "Подключение до получения конфигурации"): without this, a
            // join never finishes because `self.config` stays None forever.
            if let Some(js) = &self.joining {
                if addr == js.master_addr {
                    let game_name = js.game_name.clone();
                    if let Some(g) = msg.announcement().games.iter().find(|g| g.game_name() == game_name) {
                        self.config = Some(GameConfig::from_proto(&g.config));
                    }
                }
            }
            return;
        }
        if msg.has_ack() {
            self.handle_ack(&msg, addr);
            return;
        }

        // Everything else requires per-message handling, possibly before we
        // have a game context (join flow), possibly as MASTER.
        if self.joining.is_some() {
            self.handle_datagram_while_joining(&msg, addr);
            return;
        }

        if self.is_master() {
            self.handle_datagram_as_master(&msg, addr);
        } else if self.game_name.is_some() {
            self.handle_datagram_as_participant(&msg, addr);
        }
    }

    fn is_server_addr(&self, addr: SocketAddrV4) -> bool {
        self.course_server_addr == Some(addr)
    }

    fn reply_discover(&mut self, to: SocketAddrV4) {
        if let Some(game) = &self.game {
            if !self.is_master() {
                return;
            }
            let mut m = transport::make_envelope(0, None, None, None);
            let mut ann = proto::game_message::AnnouncementMsg::new();
            ann.games.push(self.build_announcement(game));
            m.set_announcement(ann);
            if let Some(bytes) = transport::encode(&m) {
                let _ = self.sockets_unicast.send_to(&bytes, SocketAddr::V4(to));
            }
        }
    }

    fn build_announcement(&self, game: &Game) -> proto::GameAnnouncement {
        let mut a = proto::GameAnnouncement::new();
        let gs = game.to_proto();
        a.players = gs.players.clone();
        a.config = protobuf::MessageField::some(game.config.to_proto());
        a.set_can_join(game.snakes.len() < crate::model::MAX_SNAKES && game.players.len() < MAX_PLAYERS);
        a.set_game_name(self.game_name.clone().unwrap_or_default());
        a
    }

    fn handle_ack(&mut self, msg: &GameMessage, addr: SocketAddrV4) {
        if let Some(js) = &mut self.joining {
            if addr == js.master_addr && msg.msg_seq() == js.join_seq {
                js.acked = true;
                if msg.has_receiver_id() {
                    js.assigned_id = Some(msg.receiver_id());
                }
            }
        }
        if let Some(r) = self.remotes.values_mut().find(|r| r.addr == addr) {
            r.peer.ack(msg.msg_seq());
            r.peer.last_unicast_received = Instant::now();
        }
    }

    // ---------------------------------------------------------------
    // Joining-phase message handling (pre-registration)
    // ---------------------------------------------------------------

    fn handle_datagram_while_joining(&mut self, msg: &GameMessage, addr: SocketAddrV4) {
        let Some(js) = &mut self.joining else { return };
        if addr != js.master_addr {
            return;
        }
        if msg.has_error() {
            if msg.error().request_seq() == js.join_seq {
                self.error = Some(msg.error().error_message().to_string());
                self.joining = None;
                self.mode = Mode::Browsing;
            }
            return;
        }
        if msg.has_state() {
            let order = msg.state().state.state_order();
            if order > self.last_state_order {
                let state = (*msg.state().state).clone();
                if js.assigned_id.is_none() {
                    // Peek our id from the players list by matching endpoint
                    // is not possible pre-registration from our own view, so
                    // we just buffer; receiver_id arriving via ACK will match.
                }
                js.buffered_state = Some((order, state));
                self.last_state_order = order;
            }
            self.ack_unicast(msg, addr);
            return;
        }
        if msg.has_ping() {
            self.ack_unicast(msg, addr);
        }
    }

    fn ack_unicast(&mut self, msg: &GameMessage, addr: SocketAddrV4) {
        let ack = transport::make_ack(msg, self.my_id, msg.sender_id.map(|_| msg.sender_id()));
        if let Some(bytes) = transport::encode(&ack) {
            let _ = self.sockets_unicast.send_to(&bytes, SocketAddr::V4(addr));
        }
    }

    fn send_best_effort(&self, msg: &GameMessage, addr: SocketAddrV4) {
        if let Some(bytes) = transport::encode(msg) {
            let _ = self.sockets_unicast.send_to(&bytes, SocketAddr::V4(addr));
        }
    }

    // ---------------------------------------------------------------
    // MASTER-side handling
    // ---------------------------------------------------------------

    fn handle_datagram_as_master(&mut self, msg: &GameMessage, addr: SocketAddrV4) {
        // Resolve the sending participant, if already registered.
        let existing_id = self.remotes.values().find(|r| r.addr == addr).map(|r| r.id);

        if msg.has_join() {
            self.master_handle_join(msg, addr, existing_id);
            return;
        }

        let Some(sender_id) = existing_id else { return };

        // Reply-cache dedup for requests requiring ACK/Error.
        if msg.has_steer() || msg.has_player_status_change() || msg.has_leave() {
            if let Some(r) = self.remotes.get_mut(&sender_id) {
                if !r.peer.reply_cache.in_window(msg.msg_seq()) {
                    return;
                }
                if let Some(cached) = r.peer.reply_cache.lookup(msg.msg_seq()) {
                    let cached = cached.to_vec();
                    let _ = self.sockets_unicast.send_to(&cached, SocketAddr::V4(addr));
                    return;
                }
            }
        }

        if msg.has_ping() {
            self.ack_unicast(msg, addr);
            if let Some(r) = self.remotes.get_mut(&sender_id) {
                r.peer.last_unicast_received = Instant::now();
            }
            return;
        }
        if msg.has_steer() {
            self.master_handle_steer(msg, sender_id, addr);
            return;
        }
        if msg.has_player_status_change() {
            self.master_handle_status_change(msg, sender_id, addr);
            return;
        }
        if msg.has_leave() {
            self.master_handle_leave(msg, sender_id, addr);
            return;
        }
        // Any other valid unicast updates liveness.
        if let Some(r) = self.remotes.get_mut(&sender_id) {
            r.peer.last_unicast_received = Instant::now();
            self.ack_unicast(msg, addr);
        }
    }

    fn master_handle_join(&mut self, msg: &GameMessage, addr: SocketAddrV4, existing_id: Option<i32>) {
        let join = msg.join();
        let Some(game_name) = self.game_name.clone() else { return };
        if join.game_name() != game_name {
            return;
        }

        if let Some(id) = existing_id {
            // Already connected: ACK with the same id, no new snake.
            if let Some(r) = self.remotes.get_mut(&id) {
                if r.peer.reply_cache.in_window(msg.msg_seq()) {
                    if let Some(cached) = r.peer.reply_cache.lookup(msg.msg_seq()) {
                        let cached = cached.to_vec();
                        let _ = self.sockets_unicast.send_to(&cached, SocketAddr::V4(addr));
                        return;
                    }
                    let mut ack = transport::make_ack(msg, Some(id), Some(id));
                    ack.set_receiver_id(id);
                    if let Some(bytes) = transport::encode(&ack) {
                        self.remotes.get_mut(&id).unwrap().peer.reply_cache.record(msg.msg_seq(), bytes.clone());
                        let _ = self.sockets_unicast.send_to(&bytes, SocketAddr::V4(addr));
                    }
                }
            }
            return;
        }

        let game = self.game.as_mut().unwrap();
        if game.players.len() >= MAX_PLAYERS {
            self.send_join_error(msg, addr, "Игра заполнена");
            return;
        }
        let requested = PlayerStatus::from_proto(join.requested_status());
        let Some(id) = game.alloc_player_id() else {
            self.send_join_error(msg, addr, "Достигнут предел идентификаторов");
            return;
        };
        if matches!(requested, PlayerStatus::Playing) {
            if let Err(e) = game.join_player(id) {
                let text = match e {
                    JoinError::NoSpace => "Нет места для змейки",
                    JoinError::TooManyPlayers => "Достигнут предел змеек",
                };
                self.send_join_error(msg, addr, text);
                return;
            }
        }
        let name = truncate_utf8(join.player_name(), MAX_PLAYER_NAME_BYTES);
        game.players.insert(
            id,
            Player {
                id,
                name,
                addr: Some(addr),
                role: NodeRole::Normal,
                score: 0,
                status: requested,
            },
        );
        let mut remote = Remote::new(id, addr);
        remote.peer.last_unicast_received = Instant::now();
        self.remotes.insert(id, remote);
        self.maybe_assign_deputy();
        let game = self.game.as_mut().unwrap();
        game.state_order += 1;
        self.last_state_order = game.state_order;

        let mut ack = transport::make_ack(msg, Some(id), None);
        ack.set_receiver_id(id);
        if let Some(bytes) = transport::encode(&ack) {
            if let Some(r) = self.remotes.get_mut(&id) {
                r.peer.reply_cache.record(msg.msg_seq(), bytes.clone());
            }
            let _ = self.sockets_unicast.send_to(&bytes, SocketAddr::V4(addr));
        }
        self.publish_state();
    }

    fn send_join_error(&self, msg: &GameMessage, addr: SocketAddrV4, text: &str) {
        let mut m = transport::make_envelope(0, self.my_id, None, self.game_name.as_deref());
        let mut err = proto::game_message::ErrorMsg::new();
        err.set_error_message(truncate_utf8(text, MAX_ERROR_BYTES));
        err.set_request_seq(msg.msg_seq());
        m.set_error(err);
        self.send_best_effort(&m, addr);
    }

    fn master_handle_steer(&mut self, msg: &GameMessage, sender_id: i32, addr: SocketAddrV4) {
        self.ack_unicast(msg, addr);
        if let Some(r) = self.remotes.get_mut(&sender_id) {
            r.peer.last_unicast_received = Instant::now();
        }
        let dir = Direction::from_proto(msg.steer().direction());
        self.requested_moves.insert(sender_id, dir);
    }

    fn master_handle_status_change(&mut self, msg: &GameMessage, sender_id: i32, addr: SocketAddrV4) {
        let requested = PlayerStatus::from_proto(msg.player_status_change().status());
        let game = self.game.as_mut().unwrap();
        let current = game.players.get(&sender_id).map(|p| p.status);
        let mut changed = false;
        let mut reply_ok = true;
        let mut error_text = None;
        match (current, requested) {
            (Some(PlayerStatus::Playing), PlayerStatus::Viewing) => {
                game.snakes.remove(&sender_id);
                if let Some(p) = game.players.get_mut(&sender_id) {
                    p.status = PlayerStatus::Viewing;
                }
                changed = true;
            }
            (Some(PlayerStatus::Viewing), PlayerStatus::Playing) => {
                match game.join_player(sender_id) {
                    Ok(()) => {
                        if let Some(p) = game.players.get_mut(&sender_id) {
                            p.status = PlayerStatus::Playing;
                        }
                        changed = true;
                    }
                    Err(e) => {
                        reply_ok = false;
                        error_text = Some(match e {
                            JoinError::NoSpace => "Нет места для змейки",
                            JoinError::TooManyPlayers => "Достигнут предел змеек",
                        });
                    }
                }
            }
            _ => {}
        }
        if reply_ok {
            let ack = transport::make_ack(msg, self.my_id, Some(sender_id));
            if let Some(bytes) = transport::encode(&ack) {
                if let Some(r) = self.remotes.get_mut(&sender_id) {
                    r.peer.reply_cache.record(msg.msg_seq(), bytes.clone());
                    r.peer.last_unicast_received = Instant::now();
                }
                let _ = self.sockets_unicast.send_to(&bytes, SocketAddr::V4(addr));
            }
            if changed {
                game.state_order += 1;
                self.last_state_order = game.state_order;
                self.publish_state();
            } else {
                self.publish_state_to(addr, sender_id);
            }
        } else {
            self.send_join_error(msg, addr, error_text.unwrap_or("Ошибка"));
        }
    }

    fn master_handle_leave(&mut self, msg: &GameMessage, sender_id: i32, addr: SocketAddrV4) {
        let ack = transport::make_ack(msg, self.my_id, Some(sender_id));
        if let Some(bytes) = transport::encode(&ack) {
            if let Some(r) = self.remotes.get_mut(&sender_id) {
                r.peer.reply_cache.record(msg.msg_seq(), bytes.clone());
            }
            let _ = self.sockets_unicast.send_to(&bytes, SocketAddr::V4(addr));
        }
        self.remove_participant(sender_id);
    }

    fn remove_participant(&mut self, id: i32) {
        if let Some(game) = &mut self.game {
            game.snakes.remove(&id);
            game.players.remove(&id);
            game.state_order += 1;
            self.last_state_order = game.state_order;
        }
        self.remotes.remove(&id);
        if self.deputy_id == Some(id) {
            self.deputy_id = None;
            self.deputy_addr = None;
            self.maybe_assign_deputy();
        }
        if self.game.as_ref().map(|g| g.players.is_empty()).unwrap_or(false) {
            self.should_quit_game_over();
            return;
        }
        self.publish_state();
    }

    fn should_quit_game_over(&mut self) {
        self.reset_game_state();
    }

    fn maybe_assign_deputy(&mut self) {
        if self.deputy_id.is_some() {
            return;
        }
        let Some(game) = &self.game else { return };
        let candidate = game
            .players
            .values()
            .find(|p| p.id != self.my_id.unwrap_or(-1) && matches!(p.role, NodeRole::Normal))
            .map(|p| p.id);
        let Some(cand) = candidate else { return };
        if let Some(p) = self.game.as_mut().unwrap().players.get_mut(&cand) {
            p.role = NodeRole::Deputy;
        }
        if let Some(r) = self.remotes.get(&cand) {
            self.deputy_addr = Some(r.addr);
        }
        self.deputy_id = Some(cand);
        let order = self.game.as_ref().unwrap().state_order;
        self.send_node_role_change(cand, None, Some(proto::NodeRole::DEPUTY), order);
    }

    fn send_node_role_change(
        &mut self,
        to_id: i32,
        sender_role: Option<proto::NodeRole>,
        receiver_role: Option<proto::NodeRole>,
        state_order: i32,
    ) {
        let Some(addr) = self.remotes.get(&to_id).map(|r| r.addr) else { return };
        let seq = self.next_seq_for(to_id);
        let mut m = transport::make_envelope(seq, self.my_id, Some(to_id), self.game_name.as_deref());
        let mut nrc = proto::game_message::NodeRoleChangeMsg::new();
        if let Some(r) = sender_role {
            nrc.set_sender_role(r);
        }
        if let Some(r) = receiver_role {
            nrc.set_receiver_role(r);
        }
        nrc.set_state_order(state_order);
        m.set_node_role_change(nrc);
        if let Some(bytes) = transport::encode(&m) {
            if let Some(r) = self.remotes.get_mut(&to_id) {
                r.peer.queue(seq, bytes.clone());
            }
            let _ = self.sockets_unicast.send_to(&bytes, SocketAddr::V4(addr));
        }
    }

    fn next_seq_for(&mut self, id: i32) -> i64 {
        self.remotes.get_mut(&id).map(|r| r.alloc_seq()).unwrap_or(0)
    }

    fn drive_master_tick(&mut self) {
        let Some(config) = self.config else { return };
        let now = Instant::now();
        let interval = Duration::from_millis(config.state_delay_ms as u64);
        if now.duration_since(self.last_tick) < interval {
            if now.duration_since(self.last_discover_broadcast) >= ANNOUNCE_INTERVAL {
                self.broadcast_announcement();
                self.last_discover_broadcast = now;
            }
            return;
        }
        self.last_tick = now;

        if let (Some(dir), Some(id)) = (self.pending_steer.take(), self.my_id) {
            if matches!(self.my_status, PlayerStatus::Playing) {
                self.requested_moves.insert(id, dir);
            }
        }

        let moves = std::mem::take(&mut self.requested_moves);
        let dead = self.game.as_mut().unwrap().step(&moves);
        for id in dead {
            if self.deputy_id == Some(id) {
                self.maybe_assign_deputy();
            }
        }
        self.game.as_mut().unwrap().state_order += 1;
        self.last_state_order = self.game.as_ref().unwrap().state_order;
        self.publish_state();
        self.broadcast_announcement();
        self.last_discover_broadcast = now;
    }

    fn broadcast_announcement(&mut self) {
        let Some(game) = &self.game else { return };
        let mut m = transport::make_envelope(0, None, None, None);
        let mut ann = proto::game_message::AnnouncementMsg::new();
        ann.games.push(self.build_announcement(game));
        m.set_announcement(ann);
        if let Some(bytes) = transport::encode(&m) {
            net::send_multicast_all_interfaces(&self.sockets_unicast, &bytes);
            // Note: per snakes_task.md, unicast-announcing to the course
            // server is an explicit "invite the bot" action, not something
            // every hosted game does automatically. Sending it unconditionally
            // here caused the course bot to join every locally hosted game.
        }
    }

    fn publish_state(&mut self) {
        let Some(game) = &self.game else { return };
        let state = game.to_proto();
        self.last_state = Some(state.clone());
        let ids: Vec<i32> = self.remotes.keys().copied().collect();
        for id in ids {
            let Some(addr) = self.remotes.get(&id).map(|r| r.addr) else { continue };
            let seq = self.next_seq_for(id);
            let mut m = transport::make_envelope(seq, self.my_id, Some(id), self.game_name.as_deref());
            let mut sm = proto::game_message::StateMsg::new();
            sm.state = protobuf::MessageField::some(state.clone());
            m.set_state(sm);
            if let Some(bytes) = transport::encode(&m) {
                if let Some(r) = self.remotes.get_mut(&id) {
                    r.peer.queue(seq, bytes.clone());
                }
                let _ = self.sockets_unicast.send_to(&bytes, SocketAddr::V4(addr));
            }
        }
    }

    fn publish_state_to(&mut self, addr: SocketAddrV4, id: i32) {
        let Some(game) = &self.game else { return };
        let state = game.to_proto();
        let seq = self.next_seq_for(id);
        let mut m = transport::make_envelope(seq, self.my_id, Some(id), self.game_name.as_deref());
        let mut sm = proto::game_message::StateMsg::new();
        sm.state = protobuf::MessageField::some(state);
        m.set_state(sm);
        if let Some(bytes) = transport::encode(&m) {
            if let Some(r) = self.remotes.get_mut(&id) {
                r.peer.queue(seq, bytes.clone());
            }
            let _ = self.sockets_unicast.send_to(&bytes, SocketAddr::V4(addr));
        }
    }

    // ---------------------------------------------------------------
    // Participant-side handling (NORMAL/DEPUTY following MASTER)
    // ---------------------------------------------------------------

    fn handle_datagram_as_participant(&mut self, msg: &GameMessage, addr: SocketAddrV4) {
        let Some(master) = self.master_addr else { return };
        if addr == master {
            self.handle_from_master(msg, addr);
            return;
        }
        // Unknown sender while in-game: ignore (no direct peer links beyond
        // MASTER/DEPUTY are expected in the star topology).
    }

    fn handle_from_master(&mut self, msg: &GameMessage, addr: SocketAddrV4) {
        if msg.has_state() {
            let order = msg.state().state.state_order();
            self.ack_unicast(msg, addr);
            if order <= self.last_state_order {
                return;
            }
            self.last_state_order = order;
            let state = (*msg.state().state).clone();
            self.apply_state_snapshot(order, &state);
            return;
        }
        if msg.has_ping() {
            self.ack_unicast(msg, addr);
            return;
        }
        if msg.has_error() {
            self.error = Some(msg.error().error_message().to_string());
            self.ack_unicast(msg, addr);
            return;
        }
        if msg.has_node_role_change() {
            self.ack_unicast(msg, addr);
            let nrc = msg.node_role_change();
            if nrc.state_order() <= self.last_state_order && nrc.state_order() != 0 {
                // Stale notification: ACK'd above, do not apply.
            }
            if nrc.has_receiver_role() && nrc.receiver_role() == proto::NodeRole::DEPUTY {
                self.my_role = NodeRole::Deputy;
            }
            if nrc.has_sender_role() && nrc.sender_role() == proto::NodeRole::MASTER {
                // The old DEPUTY is announcing it is the new MASTER.
                self.master_addr = Some(addr);
                self.deputy_addr = None;
                self.deputy_id = None;
            }
            return;
        }
    }

    fn apply_state_snapshot(&mut self, order: i32, state: &proto::GameState) {
        self.last_state = Some(state.clone());
        self.last_state_order = order;
        for p in &state.players.players {
            if Some(p.id()) == self.my_id {
                self.my_status = PlayerStatus::from_proto(p.status());
                self.my_role = NodeRole::from_proto(p.role());
            }
            if p.role() == proto::NodeRole::MASTER {
                if let (Some(ip), Some(port)) = (
                    p.ip_address.as_ref(),
                    p.port,
                ) {
                    if let Ok(v4) = ip.parse::<std::net::Ipv4Addr>() {
                        self.master_addr = Some(SocketAddrV4::new(v4, port as u16));
                    }
                }
            }
            if p.role() == proto::NodeRole::DEPUTY {
                self.deputy_id = Some(p.id());
                if let (Some(ip), Some(port)) = (p.ip_address.as_ref(), p.port) {
                    if let Ok(v4) = ip.parse::<std::net::Ipv4Addr>() {
                        self.deputy_addr = Some(SocketAddrV4::new(v4, port as u16));
                    }
                }
            }
        }
    }

    fn request_toggle_status(&mut self) {
        if self.game_name.is_none() {
            return;
        }
        let target = match self.my_status {
            PlayerStatus::Playing => PlayerStatus::Viewing,
            PlayerStatus::Viewing => PlayerStatus::Playing,
        };
        if self.is_master() {
            if let Some(id) = self.my_id {
                let game = self.game.as_mut().unwrap();
                match target {
                    PlayerStatus::Viewing => {
                        game.snakes.remove(&id);
                        if let Some(p) = game.players.get_mut(&id) {
                            p.status = PlayerStatus::Viewing;
                        }
                        self.my_status = PlayerStatus::Viewing;
                        game.state_order += 1;
                        self.last_state_order = game.state_order;
                        self.publish_state();
                    }
                    PlayerStatus::Playing => {
                        if game.join_player(id).is_ok() {
                            if let Some(p) = game.players.get_mut(&id) {
                                p.status = PlayerStatus::Playing;
                            }
                            self.my_status = PlayerStatus::Playing;
                            game.state_order += 1;
                            self.last_state_order = game.state_order;
                            self.publish_state();
                        } else {
                            self.error = Some("Нет места для змейки".into());
                        }
                    }
                }
            }
            return;
        }
        let Some(master) = self.master_addr else { return };
        let Some(id) = self.my_id else { return };
        let seq = self.alloc_my_seq();
        let mut m = transport::make_envelope(seq, Some(id), None, self.game_name.as_deref());
        let mut psc = proto::game_message::PlayerStatusChangeMsg::new();
        psc.set_status(target.to_proto());
        m.set_player_status_change(psc);
        self.send_best_effort(&m, master);
    }

    // ---------------------------------------------------------------
    // Pings, retries and failover timeouts
    // ---------------------------------------------------------------

    fn drive_ping_and_timeouts(&mut self) {
        let now = Instant::now();

        if self.is_master() {
            let dead: Vec<i32> = self
                .remotes
                .iter()
                .filter(|(_, r)| r.peer.is_timed_out(now))
                .map(|(id, _)| *id)
                .collect();
            for id in dead {
                self.remove_participant(id);
            }
            let ids: Vec<i32> = self.remotes.keys().copied().collect();
            for id in ids {
                if let Some(r) = self.remotes.get_mut(&id) {
                    if now.duration_since(r.peer.last_unicast_sent) >= PING_INTERVAL
                        && !r.peer.has_pending()
                    {
                        let seq = r.alloc_seq();
                        let addr = r.addr;
                        let mut m = transport::make_envelope(seq, self.my_id, Some(id), self.game_name.as_deref());
                        m.set_ping(proto::game_message::PingMsg::new());
                        if let Some(bytes) = transport::encode(&m) {
                            let _ = self.sockets_unicast.send_to(&bytes, SocketAddr::V4(addr));
                        }
                        if let Some(r) = self.remotes.get_mut(&id) {
                            r.peer.last_unicast_sent = now;
                        }
                    }
                }
            }
            return;
        }

        // Non-MASTER participants: send pending steer and ping the MASTER.
        if self.game_name.is_some() {
            if let Some(master) = self.master_addr {
                let timed_out = now.duration_since(self.last_master_contact()) > PEER_TIMEOUT;
                if timed_out {
                    self.handle_master_lost();
                    return;
                }
                // Send SteerMsg if the user has requested a direction change.
                if let (Some(dir), Some(my_id)) = (self.pending_steer.take(), self.my_id) {
                    if matches!(self.my_status, PlayerStatus::Playing) {
                        let seq = self.alloc_my_seq();
                        let mut m = transport::make_envelope(seq, Some(my_id), None, self.game_name.as_deref());
                        let mut steer = proto::game_message::SteerMsg::new();
                        steer.set_direction(dir.to_proto());
                        m.set_steer(steer);
                        self.send_best_effort(&m, master);
                        self.mark_sent_to_master(now);
                    }
                }
                if now.duration_since(self.last_sent_to_master()) >= PING_INTERVAL {
                    let seq = self.alloc_my_seq();
                    let mut m = transport::make_envelope(seq, self.my_id, None, self.game_name.as_deref());
                    m.set_ping(proto::game_message::PingMsg::new());
                    self.send_best_effort(&m, master);
                    self.mark_sent_to_master(now);
                }
            }
        }
    }

    // Minimal per-master liveness bookkeeping reusing a synthetic remote slot
    // keyed by -1 so the rest of the retry/ack machinery can be reused.
    fn last_master_contact(&self) -> Instant {
        self.remotes.get(&-1).map(|r| r.peer.last_unicast_received).unwrap_or_else(Instant::now)
    }
    fn last_sent_to_master(&self) -> Instant {
        self.remotes.get(&-1).map(|r| r.peer.last_unicast_sent).unwrap_or_else(|| Instant::now() - PING_INTERVAL)
    }
    fn mark_sent_to_master(&mut self, now: Instant) {
        self.remotes.entry(-1).or_insert_with(|| Remote::new(-1, self.master_addr.unwrap())).peer.last_unicast_sent = now;
    }

    fn handle_master_lost(&mut self) {
        if self.is_deputy() {
            self.become_master_after_failover();
            return;
        }
        if let Some(dep) = self.deputy_addr {
            self.master_addr = Some(dep);
            self.remotes.remove(&-1);
            return;
        }
        self.error = Some("Соединение с игрой потеряно".into());
        self.reset_game_state();
    }

    fn become_master_after_failover(&mut self) {
        self.my_role = NodeRole::Master;
        let Some(state) = self.last_state.clone() else {
            self.reset_game_state();
            return;
        };
        let config = self.config.unwrap_or_default();
        let mut game = Game::new(config);
        game.state_order = state.state_order() + 1;
        game.next_player_id = state.next_player_id();
        for p in &state.players.players {
            if Some(p.id()) == self.master_addr.and(None::<i32>) {
                // unreachable, placeholder to keep structure simple
            }
        }
        // Rebuild players/snakes from the last snapshot, dropping the old MASTER.
        let old_master_id = state
            .players
            .players
            .iter()
            .find(|p| p.role() == proto::NodeRole::MASTER)
            .map(|p| p.id());
        for p in &state.players.players {
            if Some(p.id()) == old_master_id {
                continue;
            }
            let role = if Some(p.id()) == self.my_id {
                NodeRole::Master
            } else if Some(p.id()) == self.deputy_id {
                NodeRole::Normal
            } else {
                NodeRole::from_proto(p.role())
            };
            let addr = if Some(p.id()) == self.my_id {
                None
            } else if p.has_ip_address() && p.has_port() {
                p.ip_address()
                    .parse::<std::net::Ipv4Addr>()
                    .ok()
                    .map(|ip| SocketAddrV4::new(ip, p.port() as u16))
            } else {
                None
            };
            game.players.insert(
                p.id(),
                Player {
                    id: p.id(),
                    name: p.name().to_string(),
                    addr,
                    role,
                    score: p.score(),
                    status: PlayerStatus::from_proto(p.status()),
                },
            );
            if let Some(a) = addr {
                self.remotes.insert(p.id(), Remote::new(p.id(), a));
            }
        }
        for s in &state.snakes {
            if let Some(snake) = crate::game::Snake::from_proto(s) {
                if game.players.contains_key(&snake.player_id) {
                    game.snakes.insert(snake.player_id, snake);
                }
            }
        }
        for f in &state.foods {
            game.foods.insert(crate::model::Point::new(f.x(), f.y()));
        }
        self.deputy_id = None;
        self.deputy_addr = None;
        self.game = Some(game);
        self.master_addr = None;
        self.remotes.remove(&-1);
        self.maybe_assign_deputy();
        self.game.as_mut().unwrap().state_order += 1;
        self.last_state_order = self.game.as_ref().unwrap().state_order;
        self.publish_state();
    }

    fn drive_resends(&mut self) {
        let now = Instant::now();
        let ids: Vec<i32> = self.remotes.keys().copied().collect();
        for id in ids {
            let addr = self.remotes[&id].addr;
            let due = self
                .remotes
                .get_mut(&id)
                .unwrap()
                .peer
                .due_resends(now, RESEND_INTERVAL);
            for bytes in due {
                let _ = self.sockets_unicast.send_to(&bytes, SocketAddr::V4(addr));
            }
        }
    }
}

fn resolve_course_server() -> Option<SocketAddrV4> {
    use std::net::ToSocketAddrs;
    COURSE_SERVER
        .to_socket_addrs()
        .ok()?
        .find_map(|a| transport::socket_addr_v4(a))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local_addr(s: &Session) -> SocketAddrV4 {
        let a = transport::socket_addr_v4(s.sockets_unicast.local_addr().unwrap()).unwrap();
        // bind_unicast binds to 0.0.0.0; sending there from another socket
        // doesn't reliably reach it, so address it via loopback explicitly.
        SocketAddrV4::new(std::net::Ipv4Addr::LOCALHOST, a.port())
    }

    /// Reproduces the host -> join flow over real (loopback) UDP sockets,
    /// directly at the MASTER's unicast endpoint (bypassing multicast
    /// discovery, which is irrelevant to this bug). Without learning the
    /// config from the MASTER's Announcement, a joiner's `drive_join_bootstrap`
    /// never finishes and the UI reports a connection timeout.
    #[test]
    fn join_completes_with_config_and_without_tui() {
        let mut host = Session::new().unwrap();
        host.host_game(
            "t-game".to_string(),
            "host-player".to_string(),
            GameConfig {
                width: 10,
                height: 10,
                food_static: 1,
                state_delay_ms: 1000,
            },
        );
        let master_addr = local_addr(&host);

        let mut joiner = Session::new().unwrap();
        joiner.start_join(
            GameId {
                game_name: "t-game".to_string(),
                master_addr,
            },
            "joiner".to_string(),
            true,
        );

        let mut joined = false;
        for _ in 0..200 {
            host.tick();
            joiner.tick();
            if joiner.game_name.is_some() && joiner.config.is_some() {
                joined = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(joined, "joiner never finished bootstrapping (config stuck at None)");
        assert_eq!(joiner.config, host.config);
        assert!(joiner.my_id.is_some());
    }

    /// Confirms that a joined participant's Steer message reaches the MASTER
    /// and that the requested direction is applied to the participant's snake
    /// on the next tick.
    #[test]
    fn steer_message_reaches_master_and_applies() {
        let mut host = Session::new().unwrap();
        host.host_game(
            "t-steer".to_string(),
            "host-p".to_string(),
            GameConfig {
                width: 20,
                height: 20,
                food_static: 0,
                state_delay_ms: 100,
            },
        );
        let master_addr = local_addr(&host);

        let mut joiner = Session::new().unwrap();
        joiner.start_join(
            GameId {
                game_name: "t-steer".to_string(),
                master_addr,
            },
            "joiner-p".to_string(),
            true,
        );

        // Join bootstrap
        for _ in 0..200 {
            host.tick();
            joiner.tick();
            if joiner.my_id.is_some() && joiner.config.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(joiner.my_id.is_some());

        // Ensure the MASTER has fully registered the joiner (ACK processed).
        for _ in 0..10 {
            host.tick();
            joiner.tick();
            std::thread::sleep(Duration::from_millis(5));
        }

        // Pick a direction perpendicular to the joiner snake's current
        // heading on the MASTER (steering into the opposite direction is
        // correctly ignored by the game as a reversal into the neck).
        let joiner_id = joiner.my_id.unwrap();
        let before_dir = host.game.as_ref().unwrap().snakes[&joiner_id].head_dir;
        let new_dir = match before_dir {
            Direction::Up | Direction::Down => Direction::Left,
            Direction::Left | Direction::Right => Direction::Up,
        };

        // Inject a steer request from the joiner
        joiner.handle_ui_event(UiEvent::Steer(new_dir));

        // Run ticks until the MASTER advances state (> state_delay_ms) and
        // applies the requested direction to the joiner's snake.
        for _ in 0..30 {
            joiner.tick();
            host.tick();
            std::thread::sleep(Duration::from_millis(10));
        }

        // The MASTER should have applied the requested direction to the
        // joiner's snake.
        let after_dir = host.game.as_ref().unwrap().snakes[&joiner_id].head_dir;
        assert_eq!(
            after_dir, new_dir,
            "MASTER never applied the joiner's Steer message (still heading {:?})",
            after_dir
        );
    }

    /// Confirms that a joined participant's PlayerStatusChange (the V
    /// playing/viewing toggle) reaches the MASTER and is applied: the
    /// participant's snake disappears from the MASTER's game state once they
    /// switch to viewing.
    #[test]
    fn status_change_message_reaches_master_and_applies() {
        let mut host = Session::new().unwrap();
        host.host_game(
            "t-status".to_string(),
            "host-p".to_string(),
            GameConfig {
                width: 20,
                height: 20,
                food_static: 0,
                state_delay_ms: 100,
            },
        );
        let master_addr = local_addr(&host);

        let mut joiner = Session::new().unwrap();
        joiner.start_join(
            GameId {
                game_name: "t-status".to_string(),
                master_addr,
            },
            "joiner-p".to_string(),
            true,
        );

        // Join bootstrap
        for _ in 0..200 {
            host.tick();
            joiner.tick();
            if joiner.my_id.is_some() && joiner.config.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(joiner.my_id.is_some());
        let joiner_id = joiner.my_id.unwrap();

        // Ensure the MASTER has fully registered the joiner (ACK processed).
        for _ in 0..10 {
            host.tick();
            joiner.tick();
            std::thread::sleep(Duration::from_millis(5));
        }

        assert!(
            host.game.as_ref().unwrap().snakes.contains_key(&joiner_id),
            "joiner should start out Playing with a snake on the MASTER"
        );

        // Ask to switch to Viewing.
        joiner.handle_ui_event(UiEvent::ToggleStatus);

        for _ in 0..30 {
            joiner.tick();
            host.tick();
            std::thread::sleep(Duration::from_millis(10));
        }

        assert!(
            !host.game.as_ref().unwrap().snakes.contains_key(&joiner_id),
            "MASTER never applied the joiner's PlayerStatusChange (snake still present)"
        );
        assert_eq!(
            host.game.as_ref().unwrap().players[&joiner_id].status,
            PlayerStatus::Viewing
        );
    }
}
