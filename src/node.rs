//! Сетевое ядро игры: роли MASTER / DEPUTY / NORMAL, присоединение, публикация
//! состояния, смена MASTER и выход из игры.
//!
//! Ядро «чистое»: входящие датаграммы подаются в `on_datagram`, время — в
//! `on_tick`/`command`, исходящее забирается из `drain_out`. Сокетов и чтения часов
//! здесь нет, поэтому весь протокол проверяется в симуляторе сети.
//!
//! # Как читать модуль
//!
//! [`Node`] владеет каталогом, общей очередью отправки и одной из фаз:
//! `Lobby → Joining → InGame → Lobby`. `Joining` отдельно собирает ACK на Join,
//! конфигурацию из анонса и первый снимок: UDP не гарантирует порядок их прихода.
//! Подключение инкапсулировано в `joining`; переходы фаз выполняет `Node`.
//! `Game` хранит уже начатую сессию; методы `m_*` обслуживают MASTER,
//! а `p_*` — участника, следующего за ним (NORMAL или DEPUTY).
//!
//! Два порядка нельзя смешивать: `msg_seq` относится к доставке сообщений и
//! новизне команд, `state_order` — к версии мира. Например, запоздалый State
//! можно подтвердить, не применяя его. Сетевую доставку ведёт [`Reliable`],
//! движение рассчитывает [`World`], а этот модуль связывает их с ролями и статусами.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::time::{Duration, Instant};

use crate::catalog::{Catalog, Entry, GameKey, Source};
use crate::reliable::{Fault, Kind, Out, Outbox, Reliable, Seen, describe};
use crate::rules::{Rng, Snake, World};
use crate::wire::*;

mod joining;
use joining::{JoinStep, Joining};

/// Общий срок на присоединение: ACK, конфигурация и первое состояние.
pub const JOIN_DEADLINE: Duration = Duration::from_millis(5000);
pub const ANNOUNCE_PERIOD: Duration = Duration::from_millis(1000);
pub const DISCOVER_PERIOD: Duration = Duration::from_millis(1000);
pub const JOIN_DISCOVER_PERIOD: Duration = Duration::from_millis(500);
pub const LEAVE_DEADLINE: Duration = Duration::from_millis(1000);

// ───────────────────────────── Публичный интерфейс ───────────────────────────

/// Действие пользователя; узел сам решает, допустимо ли оно в текущей фазе.
#[derive(Clone, Debug)]
pub enum Command {
    /// Создать игру, ведущим которой станет этот узел.
    Host {
        game: String,
        player: String,
        cfg: Config,
    },
    /// Создать игру на сервере курса: Join с новым именем.
    HostOnServer {
        game: String,
        player: String,
    },
    Join {
        key: GameKey,
        player: String,
        status: PlayerStatus,
    },
    Steer(Dir),
    /// Перейти из игрока в наблюдатели и обратно.
    ToggleWatch,
    /// Выйти из игры в лобби.
    Leave,
    /// Пригласить робота: отправить анонс игры серверу курса.
    InviteRobot,
    /// Выйти из игры и завершить программу.
    Quit,
}

/// Уведомление для интерфейса; оно не заменяет чтение актуального [`GameView`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// Информационное сообщение для строки состояния.
    Notice(String),
    /// Ошибка с причиной: присоединение не удалось, связь потеряна и т. п.
    Failure(String),
    Joined,
    /// Игра для этого узла закончилась.
    GameOver(String),
    /// Можно закрывать программу.
    Exited,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PhaseKind {
    Lobby,
    Joining,
    InGame,
}

/// Заимствованное представление сессии для TUI без доступа к изменению мира.
///
/// `role` задаёт сетевые обязанности, `status` — участие в игре. `status_pending`
/// отражает ожидание ответа на запрос, а фактический статус берётся из состояния.
pub struct GameView<'a> {
    pub name: &'a str,
    pub cfg: Config,
    pub world: &'a World,
    pub players: &'a [PlayerInfo],
    pub my_id: i32,
    pub role: NodeRole,
    pub status: PlayerStatus,
    pub order: i32,
    pub leaving: bool,
    pub master: Option<SocketAddrV4>,
    /// Отправлена, но ещё не подтверждена смена статуса.
    pub status_pending: bool,
}

pub struct JoiningView<'a> {
    pub game: &'a str,
    pub started: Instant,
    pub recovering: bool,
    pub acked: bool,
    pub has_config: bool,
}

// ─────────────────────────────── Внутренние типы ─────────────────────────────

/// Временные зависимости одного вызова: время, исходящие пакеты, события и RNG.
/// Отдельный контекст позволяет менять фазу, не передавая ей весь `Node`.
struct Ctx<'a> {
    now: Instant,
    ob: &'a mut Outbox,
    ev: &'a mut Vec<Event>,
    rng: &'a mut Rng,
}

impl Ctx<'_> {
    fn notice(&mut self, s: impl Into<String>) {
        self.ev.push(Event::Notice(s.into()));
    }
}

/// Результат обработки внутри игры; переход в лобби выполняет `Node::finish_flow`.
enum Flow {
    Continue,
    /// Игра закончилась по внешней причине.
    Ended(String),
    /// Игра закончилась ошибкой.
    Failed(String),
    /// Штатный выход завершён.
    Left,
}

impl Flow {
    fn exhausted_order() -> Self {
        Flow::Ended("Исчерпан диапазон state_order".into())
    }
}

/// Сообщение о выходе, которое ещё повторяется после ухода из игры.
struct Closing {
    to: SocketAddrV4,
    msg: Message,
    last_sent: Instant,
    deadline: Instant,
}

enum Phase {
    Lobby,
    Joining(Box<Joining>),
    InGame(Box<Game>),
}

/// Передача управления при выходе MASTER с общим дедлайном на обе стадии.
struct Leaving {
    stage: HandoverStage,
    deadline: Instant,
    to: SocketAddrV4,
}

#[derive(Clone, Copy)]
enum HandoverStage {
    State(i64),
    RoleChange(i64),
    Complete,
}

/// Данные, используемые только ведущим; при повышении DEPUTY создаются заново.
struct MasterState {
    next_tick: Instant,
    next_announce: Instant,
    /// Последняя допустимая команда курса на текущий ход: (msg_seq, направление).
    steers: HashMap<i32, (i64, Dir)>,
    /// Номера команд сохраняются между ходами, в отличие от очищаемого `steers`.
    steer_seq: HashMap<i32, i64>,
    /// Независимый порядок запросов статуса, не связанный с порядком поворотов.
    status_seq: HashMap<i32, i64>,
    /// Назначенный DEPUTY, которому следующая публикация пошлёт RoleChange.
    deputy_notice: Option<i32>,
}

impl MasterState {
    fn new(now: Instant, delay: Duration) -> Self {
        MasterState {
            next_tick: now + delay,
            next_announce: now,
            steers: HashMap::new(),
            steer_seq: HashMap::new(),
            status_seq: HashMap::new(),
            deputy_notice: None,
        }
    }
}

/// Общая копия сессии; у MASTER она авторитетна, у остальных обновляется из State.
/// DEPUTY хранит те же данные, включая `next_id`, чтобы продолжить игру при отказе.
struct Game {
    name: String,
    my_id: i32,
    role: NodeRole,
    status: PlayerStatus,
    world: World,
    players: Vec<PlayerInfo>,
    order: i32,
    next_id: i32,
    /// Endpoint ведущего; у самого ведущего `None`.
    master_addr: Option<SocketAddrV4>,
    /// Уже переключились на заместителя и ждём ответа от него.
    failover_used: bool,
    rel: Reliable,
    m: MasterState,
    leaving: Option<Leaving>,
}

fn mk(
    seq: i64,
    sender: Option<i32>,
    receiver: Option<i32>,
    game: Option<&str>,
    body: Body,
) -> Message {
    Message {
        seq,
        sender_id: sender,
        receiver_id: receiver,
        game_name: game.map(str::to_owned),
        body,
    }
}

/// Принимает входящий запрос с подтверждением: проверяет окно дедупликации,
/// повторяет сохранённый ответ на дубликат и отправляет ACK на новый запрос.
/// Возвращает `true`, если запрос нужно обработать. Вызывающий уже должен
/// проверить отправителя и контекст; ACK сюда не передаётся. Для операций,
/// которые могут завершиться Error вместо ACK, ответ выбирается отдельно.
fn accept_request(
    rel: &mut Reliable,
    cx: &mut Ctx,
    from: SocketAddrV4,
    msg: &Message,
    my_id: Option<i32>,
    peer_id: Option<i32>,
    game: &str,
) -> bool {
    match rel.classify(from, msg.seq) {
        Seen::TooOld => false,
        Seen::Duplicate(reply) => {
            if let Some(r) = reply {
                rel.resend_reply(cx.ob, cx.now, from, &r);
            }
            false
        }
        Seen::New => {
            if !rel.can_admit(from) {
                return false;
            }
            let ack = rel.send_ack(cx.ob, cx.now, from, msg.seq, (my_id, peer_id), game);
            rel.record(from, msg.seq, Some(ack));
            true
        }
    }
}

/// Проверяет снимок состояния и превращает его в мир. Проверка намеренно
/// мягкая в деталях правил: цель — не пропустить мусор, а не отвергнуть
/// корректное состояние чужой реализации.
fn validate_state(
    cfg: Config,
    s: &StateMsg,
) -> Result<(World, Vec<PlayerInfo>, i32), &'static str> {
    if s.order < 0 {
        return Err("отрицательный state_order");
    }
    if s.players.is_empty() || s.players.len() > MAX_PLAYERS {
        return Err("неверное число участников");
    }
    if s.snakes.len() > MAX_SNAKES {
        return Err("слишком много змеек");
    }
    let mut ids: Vec<i32> = s.players.iter().map(|p| p.id).collect();
    ids.sort_unstable();
    ids.dedup();
    if ids.len() != s.players.len() {
        return Err("повторяющиеся ID участников");
    }
    if s.players
        .iter()
        .filter(|p| p.role == NodeRole::Master)
        .count()
        != 1
    {
        return Err("в состоянии должен быть ровно один MASTER");
    }
    if s.players
        .iter()
        .filter(|p| p.role == NodeRole::Deputy)
        .count()
        > 1
    {
        return Err("в состоянии больше одного DEPUTY");
    }
    let mut world = World::new(cfg);
    for ws in &s.snakes {
        if !s.players.iter().any(|p| p.id == ws.player_id) {
            return Err("змейка без владельца");
        }
        if world.snake(ws.player_id).is_some() {
            return Err("две змейки одного игрока");
        }
        world
            .snakes
            .push(Snake::from_wire(ws, cfg.width, cfg.height).ok_or("некорректная змейка")?);
    }
    world.foods = s.foods.iter().map(|&c| world.wrap(c)).collect();
    let max_id = ids.last().copied().unwrap_or(0);
    let next_id = s.next_player_id.max(max_id.saturating_add(1));
    Ok((world, s.players.clone(), next_id))
}

// ───────────────────────────────── Игра ──────────────────────────────────────

impl Game {
    fn new_master(cx: &mut Ctx, name: String, player: String, cfg: Config) -> Result<Game, String> {
        if !valid_game_name(&name) {
            return Err(format!("Недопустимое имя игры (1–{MAX_GAME_NAME} байт)"));
        }
        if !valid_player_name(&player) {
            return Err(format!(
                "Недопустимое имя игрока (1–{MAX_PLAYER_NAME} байт)"
            ));
        }
        if !cfg.is_valid() {
            return Err("Параметры игры вне допустимых границ".into());
        }
        let mut world = World::new(cfg);
        if !world.place_snake(1, cx.rng) {
            return Err("Не удалось разместить змейку на поле".into());
        }
        world.refill_food(cx.rng);
        let me = PlayerInfo {
            name: player,
            id: 1,
            addr: None,
            role: NodeRole::Master,
            ptype: PlayerType::Human,
            score: 0,
            status: PlayerStatus::Playing,
        };
        let delay = Duration::from_millis(cfg.state_delay_ms as u64);
        Ok(Game {
            name,
            my_id: 1,
            role: NodeRole::Master,
            status: PlayerStatus::Playing,
            world,
            players: vec![me],
            order: 0,
            next_id: 2,
            master_addr: None,
            failover_used: false,
            rel: Reliable::new(),
            m: MasterState::new(cx.now, delay),
            leaving: None,
        })
    }

    fn cfg(&self) -> Config {
        self.world.cfg
    }

    fn msg(&self, receiver: Option<i32>, body: Body) -> Message {
        mk(0, Some(self.my_id), receiver, Some(&self.name), body)
    }

    fn master_id(&self) -> Option<i32> {
        self.players
            .iter()
            .find(|p| p.role == NodeRole::Master)
            .map(|p| p.id)
    }

    fn deputy(&self) -> Option<&PlayerInfo> {
        self.players
            .iter()
            .find(|p| p.role == NodeRole::Deputy && p.id != self.my_id)
    }

    /// Собирает снимок, опуская собственный endpoint согласно формату протокола.
    /// Счётчик `next_id` передаётся даже после выхода игроков: ID не переиспользуются.
    fn build_state(&self) -> StateMsg {
        StateMsg {
            order: self.order,
            snakes: self
                .world
                .snakes
                .iter()
                .map(|s| s.to_wire(self.cfg().width, self.cfg().height))
                .collect(),
            foods: self.world.foods.clone(),
            players: self
                .players
                .iter()
                .map(|p| {
                    let mut p = p.clone();
                    if p.id == self.my_id {
                        p.addr = None;
                    }
                    p
                })
                .collect(),
            next_player_id: self.next_id,
        }
    }

    fn announcement(&self) -> Announcement {
        Announcement {
            players: self.build_state().players,
            config: self.cfg(),
            can_join: self.players.len() < MAX_PLAYERS,
            game_name: self.name.clone(),
        }
    }

    // ─────────────────────────── Действия ведущего ───────────────────────────

    /// Публикует новое состояние всем участникам, увеличивая `state_order`.
    /// Публикация между ходами не двигает змеек и не меняет `next_tick`.
    fn publish(&mut self, cx: &mut Ctx) -> Flow {
        let Some(next) = self.order.checked_add(1) else {
            return Flow::exhausted_order();
        };
        self.order = next;
        let state = self.build_state();
        let targets: Vec<(SocketAddrV4, i32)> = self
            .players
            .iter()
            .filter(|p| p.id != self.my_id)
            .filter_map(|p| p.addr.map(|a| (a, p.id)))
            .collect();
        for (addr, id) in targets {
            let m = self.msg(Some(id), Body::State(state.clone()));
            self.rel.send_request(cx.ob, cx.now, addr, Kind::State, m);
        }
        if let Some(id) = self.m.deputy_notice.take()
            && let Some(addr) = self
                .players
                .iter()
                .find(|p| p.id == id)
                .and_then(|p| p.addr)
        {
            let m = self.msg(
                Some(id),
                Body::RoleChange {
                    sender: None,
                    receiver: Some(NodeRole::Deputy),
                    order: self.order,
                },
            );
            self.rel.send_request(cx.ob, cx.now, addr, Kind::Control, m);
        }
        Flow::Continue
    }

    /// Назначает DEPUTY, если его нет и есть кандидат (любой NORMAL, включая наблюдателей).
    fn ensure_deputy(&mut self) {
        if self.players.iter().any(|p| p.role == NodeRole::Deputy) {
            return;
        }
        if let Some(p) = self
            .players
            .iter_mut()
            .find(|p| p.role == NodeRole::Normal && p.addr.is_some())
        {
            p.role = NodeRole::Deputy;
            self.m.deputy_notice = Some(p.id);
        }
    }

    /// Удаляет игрока и исходящую связь, но оставляет окно сохранённых ответов.
    /// Благодаря этому повтор Leave в пределах окна получает прежний ACK даже после удаления.
    fn remove_player(&mut self, id: i32) {
        if let Some(i) = self.players.iter().position(|p| p.id == id) {
            let p = self.players.remove(i);
            if let Some(a) = p.addr {
                self.rel.links.remove(&a);
            }
        }
        self.world.remove_snake(id);
        self.m.steers.remove(&id);
    }

    /// Меняет статус участника. `Ok(true)` — статус изменился.
    fn set_status(
        &mut self,
        cx: &mut Ctx,
        id: i32,
        target: PlayerStatus,
    ) -> Result<bool, &'static str> {
        let Some(cur) = self.players.iter().find(|p| p.id == id).map(|p| p.status) else {
            return Err("участник не найден");
        };
        if cur == target {
            return Ok(false);
        }
        match target {
            PlayerStatus::Viewing => {
                self.world.remove_snake(id);
                self.m.steers.remove(&id);
            }
            PlayerStatus::Playing => {
                if self.world.snakes.len() >= MAX_SNAKES {
                    return Err("Нет места: в игре уже 4 змейки");
                }
                if !self.world.place_snake(id, cx.rng) {
                    return Err("Нет свободного места для змейки");
                }
            }
        }
        if let Some(p) = self.players.iter_mut().find(|p| p.id == id) {
            p.status = target;
        }
        if id == self.my_id {
            self.status = target;
        }
        self.world.refill_food(cx.rng);
        Ok(true)
    }

    /// Завершает изменение состава/статусов без движения: заместитель, еда, снимок.
    fn after_change(&mut self, cx: &mut Ctx) -> Flow {
        self.ensure_deputy();
        self.world.refill_food(cx.rng);
        self.publish(cx)
    }

    /// Отказывает в операции: Error имеет собственный seq, но ссылается на req_seq.
    /// Именно Error сохраняется как ответ на повтор запроса; ACK запроса не нужен.
    fn m_error(
        &mut self,
        cx: &mut Ctx,
        to: SocketAddrV4,
        receiver: Option<i32>,
        req_seq: i64,
        text: &str,
    ) {
        let m = self.msg(
            receiver,
            Body::Error {
                text: truncate_bytes(text, MAX_ERROR_TEXT),
                request_seq: req_seq,
            },
        );
        if let Some(sent) = self.rel.send_request(cx.ob, cx.now, to, Kind::Control, m) {
            self.rel.record(to, req_seq, Some(sent));
        }
    }

    /// Допуск endpoint: повтор в пределах окна возвращает прежний ответ,
    /// новый Join подключённого адреса — имеющийся ID. Новая змейка размещается
    /// только при допуске нового участника в статусе PLAYING.
    fn m_join(&mut self, cx: &mut Ctx, from: SocketAddrV4, msg: Message) -> Flow {
        match self.rel.classify(from, msg.seq) {
            Seen::TooOld => return Flow::Continue,
            Seen::Duplicate(reply) => {
                if let Some(r) = reply {
                    // Сообщение об ошибке уже повторяется из очереди; ACK повторяем сразу.
                    if matches!(r.body, Body::Ack) {
                        self.rel.resend_reply(cx.ob, cx.now, from, &r);
                    }
                }
                return Flow::Continue;
            }
            Seen::New => {}
        }
        if !self.rel.can_admit(from) {
            return Flow::Continue;
        }
        let Body::Join {
            ptype,
            player_name,
            status,
            ..
        } = msg.body
        else {
            return Flow::Continue;
        };

        // Уже подключённый endpoint получает прежний ID без новой змейки.
        if let Some(existing) = self.players.iter().find(|p| p.addr == Some(from)) {
            let id = existing.id;
            let ack = self.rel.send_ack(
                cx.ob,
                cx.now,
                from,
                msg.seq,
                (Some(self.my_id), Some(id)),
                &self.name,
            );
            self.rel.record(from, msg.seq, Some(ack));
            return Flow::Continue;
        }

        if !valid_player_name(&player_name) {
            self.m_error(cx, from, None, msg.seq, "Недопустимое имя игрока");
            return Flow::Continue;
        }
        if self.players.len() >= MAX_PLAYERS {
            self.m_error(cx, from, None, msg.seq, "В игре нет свободных мест");
            return Flow::Continue;
        }
        if self.next_id == i32::MAX {
            self.m_error(cx, from, None, msg.seq, "Исчерпаны идентификаторы игроков");
            return Flow::Continue;
        }

        let id = self.next_id;
        if status == PlayerStatus::Playing
            && (self.world.snakes.len() >= MAX_SNAKES || !self.world.place_snake(id, cx.rng))
        {
            self.m_error(
                cx,
                from,
                None,
                msg.seq,
                "Нет места для змейки: можно только наблюдать",
            );
            return Flow::Continue;
        }
        self.next_id += 1;
        self.players.push(PlayerInfo {
            name: player_name,
            id,
            addr: Some(from),
            role: NodeRole::Normal,
            ptype,
            score: 0,
            status,
        });
        let link = self.rel.link(from, cx.now);
        link.peer_id = Some(id);
        link.ping = true;
        link.last_rx = cx.now;
        let ack = self.rel.send_ack(
            cx.ob,
            cx.now,
            from,
            msg.seq,
            (Some(self.my_id), Some(id)),
            &self.name,
        );
        self.rel.record(from, msg.seq, Some(ack));
        cx.notice(format!(
            "К игре присоединился «{}»",
            self.players.last().map_or("", |p| p.name.as_str())
        ));
        self.after_change(cx)
    }

    /// Повтор Leave подтверждается и после удаления участника.
    fn m_sender(&mut self, cx: &mut Ctx, from: SocketAddrV4, msg: &Message) -> Option<PlayerInfo> {
        match self.rel.classify(from, msg.seq) {
            Seen::TooOld => return None,
            Seen::Duplicate(reply) => {
                if let Some(r) = reply {
                    self.rel.resend_reply(cx.ob, cx.now, from, &r);
                }
                return None;
            }
            Seen::New => {}
        }
        let player = self
            .players
            .iter()
            .find(|p| p.addr == Some(from))
            .cloned()?;
        if msg.sender_id != Some(player.id)
            || msg.receiver_id != Some(self.my_id)
            || !self.rel.can_admit(from)
        {
            return None;
        }
        self.rel.touch(from, cx.now);
        Some(player)
    }

    fn m_ack(&mut self, cx: &mut Ctx, to: SocketAddrV4, id: i32, seq: i64) {
        let ack = self.rel.send_ack(
            cx.ob,
            cx.now,
            to,
            seq,
            (Some(self.my_id), Some(id)),
            &self.name,
        );
        self.rel.record(to, seq, Some(ack));
    }

    fn m_steer(&mut self, player: &PlayerInfo, seq: i64, dir: Dir) {
        let last = self.m.steer_seq.get(&player.id).copied().unwrap_or(-1);
        if seq > last {
            self.m.steer_seq.insert(player.id, seq);
            // Неверный разворот учитывается в порядке сообщений, но не
            // стирает последнюю допустимую команду на этот ход.
            if player.status == PlayerStatus::Playing && self.world.is_steer_valid(player.id, dir) {
                self.m.steers.insert(player.id, (seq, dir));
            }
        }
    }

    fn m_status(
        &mut self,
        cx: &mut Ctx,
        from: SocketAddrV4,
        id: i32,
        seq: i64,
        target: PlayerStatus,
    ) -> Flow {
        let last = self.m.status_seq.get(&id).copied().unwrap_or(-1);
        if seq <= last {
            self.m_ack(cx, from, id, seq);
            return Flow::Continue;
        }
        self.m.status_seq.insert(id, seq);
        match self.set_status(cx, id, target) {
            Ok(changed) => {
                self.m_ack(cx, from, id, seq);
                if changed {
                    return self.after_change(cx);
                }
                let state = self.msg(Some(id), Body::State(self.build_state()));
                self.rel
                    .send_request(cx.ob, cx.now, from, Kind::State, state);
            }
            Err(why) => self.m_error(cx, from, Some(id), seq, why),
        }
        Flow::Continue
    }

    /// Входящие на MASTER: ACK отдельно, затем Join или запрос участника.
    /// Порядок команд курса и статуса проверяется независимо.
    fn m_message(&mut self, cx: &mut Ctx, from: SocketAddrV4, msg: Message) -> Flow {
        match &msg.body {
            Body::Ack => {
                if let Some(p) = self.rel.on_ack(cx.now, from, msg.seq) {
                    return self.on_acked(cx, from, &p.msg);
                }
                return Flow::Continue;
            }
            Body::Join { .. } => return self.m_join(cx, from, msg),
            _ => {}
        }
        let Some(player) = self.m_sender(cx, from, &msg) else {
            return Flow::Continue;
        };
        match msg.body {
            Body::Ping => self.m_ack(cx, from, player.id, msg.seq),
            Body::Steer(dir) => {
                self.m_steer(&player, msg.seq, dir);
                self.m_ack(cx, from, player.id, msg.seq);
            }
            Body::StatusChange(target) => {
                return self.m_status(cx, from, player.id, msg.seq, target);
            }
            Body::Leave => {
                self.m_ack(cx, from, player.id, msg.seq);
                cx.notice(format!("«{}» вышел из игры", player.name));
                self.remove_player(player.id);
                return self.after_change(cx);
            }
            _ => {}
        }
        Flow::Continue
    }

    /// Реакция на ACK нашего сообщения (нужна при передаче управления).
    fn on_acked(&mut self, cx: &mut Ctx, from: SocketAddrV4, acked: &Message) -> Flow {
        let Some(l) = &self.leaving else {
            return Flow::Continue;
        };
        if l.to != from {
            return Flow::Continue;
        }
        match l.stage {
            HandoverStage::State(seq) if seq == acked.seq => {
                let Some(order) = self.order.checked_add(1) else {
                    return Flow::exhausted_order();
                };
                let m = self.msg(
                    self.players
                        .iter()
                        .find(|p| p.addr == Some(from))
                        .map(|p| p.id),
                    Body::RoleChange {
                        sender: Some(NodeRole::Normal),
                        receiver: Some(NodeRole::Master),
                        order,
                    },
                );
                if let Some(sent) = self.rel.send_request(cx.ob, cx.now, from, Kind::Control, m)
                    && let Some(l) = &mut self.leaving
                {
                    l.stage = HandoverStage::RoleChange(sent.seq);
                }
            }
            HandoverStage::RoleChange(seq) if seq == acked.seq => {
                if let Some(l) = &mut self.leaving {
                    l.stage = HandoverStage::Complete;
                }
            }
            _ => {}
        }
        Flow::Continue
    }

    /// Сначала обслуживает связь, затем при наступлении срока рассчитывает ход.
    /// Даже при медленной игре повторы, Ping и удаление недоступных узлов идут чаще ходов.
    fn m_tick(&mut self, cx: &mut Ctx) -> Flow {
        let dead = self.rel.tick(cx.ob, cx.now, &self.name, Some(self.my_id));
        let mut changed = false;
        for addr in dead {
            match self.players.iter().find(|p| p.addr == Some(addr)).cloned() {
                Some(p) => {
                    cx.notice(format!("«{}» не отвечает и удалён из игры", p.name));
                    self.remove_player(p.id);
                    changed = true;
                }
                // Адресат сообщения об ошибке, который не подтвердил его.
                None => {
                    self.rel.links.remove(&addr);
                }
            }
        }
        if changed {
            let flow = self.after_change(cx);
            if !matches!(flow, Flow::Continue) {
                return flow;
            }
        }

        let delay = Duration::from_millis(self.cfg().state_delay_ms as u64);
        if cx.now >= self.m.next_tick {
            let steers: HashMap<i32, crate::wire::Dir> =
                self.m.steers.iter().map(|(&id, &(_, d))| (id, d)).collect();
            self.m.steers.clear();
            let result = self.world.step(&steers);
            for (owner, delta) in result.points {
                if let Some(p) = self.players.iter_mut().find(|p| p.id == owner) {
                    p.score = p.score.saturating_add(delta);
                }
            }
            for owner in result.dead {
                if let Some(p) = self.players.iter_mut().find(|p| p.id == owner) {
                    p.status = PlayerStatus::Viewing;
                    if p.id == self.my_id {
                        self.status = PlayerStatus::Viewing;
                        cx.notice("Ваша змейка погибла: вы наблюдаете");
                    }
                }
            }
            self.world.refill_food(cx.rng);
            let flow = self.publish(cx);
            if !matches!(flow, Flow::Continue) {
                return flow;
            }
            self.m.next_tick += delay;
            // Если сильно отстали (пауза процесса), не пытаемся нагнать пачкой ходов.
            if cx.now.saturating_duration_since(self.m.next_tick) > delay * 3 {
                self.m.next_tick = cx.now + delay;
            }
        }

        if cx.now >= self.m.next_announce
            && let Some(seq) = cx.ob.seq()
        {
            let m = mk(
                seq,
                None,
                None,
                None,
                Body::Announcement(vec![self.announcement()]),
            );
            cx.ob.multicast(&m);
            self.m.next_announce = cx.now + ANNOUNCE_PERIOD;
        }
        Flow::Continue
    }

    // ───────────────────────── Действия участника ────────────────────────────

    /// Применяет снимок состояния. `Err` — участника удалили из игры.
    /// Некорректный снимок оставляет прежний мир; новизну order проверяет вызывающий.
    /// Статус берётся из State, а не из ACK на запрос переключения режима.
    fn apply_state(&mut self, cx: &mut Ctx, s: StateMsg) -> Result<(), String> {
        let (world, players, next_id) = match validate_state(self.cfg(), &s) {
            Ok(v) => v,
            Err(why) => {
                cx.ev.push(Event::Notice(format!(
                    "Получено некорректное состояние: {why}"
                )));
                return Ok(());
            }
        };
        let Some(me) = players.iter().find(|p| p.id == self.my_id) else {
            return Err("Вас удалили из игры".into());
        };
        let old_role = self.role;
        self.status = me.status;
        if me.role != NodeRole::Master {
            self.role = me.role;
        }
        if old_role != NodeRole::Deputy && self.role == NodeRole::Deputy {
            cx.notice("Вы назначены заместителем ведущего");
        }
        self.world = world;
        self.players = players;
        self.order = s.order;
        self.next_id = next_id;
        Ok(())
    }

    /// Входящие на NORMAL/DEPUTY: мир принимается только от выбранного MASTER.
    /// Исключение для другого endpoint — объявление преемника от известного DEPUTY.
    fn p_message(&mut self, cx: &mut Ctx, from: SocketAddrV4, msg: Message) -> Flow {
        let from_master = Some(from) == self.master_addr;
        if !from_master {
            // Смена ведущего: заместитель сообщает, что взял управление на себя.
            if let Body::RoleChange {
                sender: Some(NodeRole::Master),
                order,
                ..
            } = msg.body
                && self.deputy().and_then(|d| d.addr) == Some(from)
            {
                return self.p_switch_master(cx, from, &msg, order);
            }
            return Flow::Continue;
        }
        if msg.receiver_id.is_some_and(|r| r != self.my_id) {
            return Flow::Continue;
        }
        self.failover_used = false;
        self.rel.touch(from, cx.now);
        let peer = self.master_id();

        match msg.body {
            Body::Ack => {
                self.rel.on_ack(cx.now, from, msg.seq);
            }
            Body::Ping => {
                accept_request(
                    &mut self.rel,
                    cx,
                    from,
                    &msg,
                    Some(self.my_id),
                    peer,
                    &self.name,
                );
            }
            Body::State(ref s) => {
                if accept_request(
                    &mut self.rel,
                    cx,
                    from,
                    &msg,
                    Some(self.my_id),
                    peer,
                    &self.name,
                ) && s.order > self.order
                    && let Err(why) = self.apply_state(cx, s.clone())
                {
                    return Flow::Ended(why);
                }
            }
            Body::Error { ref text, .. } => {
                if accept_request(
                    &mut self.rel,
                    cx,
                    from,
                    &msg,
                    Some(self.my_id),
                    peer,
                    &self.name,
                ) {
                    cx.ev.push(Event::Failure(text.clone()));
                }
            }
            Body::RoleChange {
                sender,
                receiver,
                order,
            } if accept_request(
                &mut self.rel,
                cx,
                from,
                &msg,
                Some(self.my_id),
                peer,
                &self.name,
            ) =>
            {
                match (sender, receiver) {
                    (Some(NodeRole::Normal), Some(NodeRole::Master))
                        if self.role == NodeRole::Deputy =>
                    {
                        return self.become_master(cx, Some(order));
                    }
                    (_, Some(NodeRole::Deputy)) if order >= self.order => {
                        self.role = NodeRole::Deputy;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        Flow::Continue
    }

    /// Заместитель взял управление: переключаем все запросы на него.
    fn p_switch_master(
        &mut self,
        cx: &mut Ctx,
        from: SocketAddrV4,
        msg: &Message,
        order: i32,
    ) -> Flow {
        let deputy_id = self.deputy().map(|d| d.id);
        if !accept_request(
            &mut self.rel,
            cx,
            from,
            msg,
            Some(self.my_id),
            deputy_id,
            &self.name,
        ) {
            return Flow::Continue;
        }
        if order < self.order {
            return Flow::Continue;
        }
        if let Some(old) = self.master_addr {
            self.rel.retarget(old, from, deputy_id, cx.now);
        }
        self.master_addr = Some(from);
        self.failover_used = false;
        cx.notice("Ведущий сменился");
        Flow::Continue
    }

    fn p_tick(&mut self, cx: &mut Ctx) -> Flow {
        let dead = self.rel.tick(cx.ob, cx.now, &self.name, Some(self.my_id));
        if let Some(m) = self.master_addr
            && dead.contains(&m)
        {
            return self.master_lost(cx);
        }

        Flow::Continue
    }

    /// Реагирует на подозрение об отказе MASTER: DEPUTY повышается, NORMAL лишь
    /// переключается на известного заместителя. Повторный тайм-аут завершает сессию.
    fn master_lost(&mut self, cx: &mut Ctx) -> Flow {
        cx.notice("Ведущий не отвечает");
        if self.role == NodeRole::Deputy {
            return self.become_master(cx, None);
        }
        if self.failover_used {
            return Flow::Failed(
                "Связь с игрой потеряна: заместитель ведущего тоже не отвечает".into(),
            );
        }
        let Some((id, addr)) = self.deputy().and_then(|d| d.addr.map(|a| (d.id, a))) else {
            return Flow::Failed("Связь с игрой потеряна: заместителя ведущего нет".into());
        };
        if let Some(old) = self.master_addr {
            self.rel.retarget(old, addr, Some(id), cx.now);
        }
        self.master_addr = Some(addr);
        self.failover_used = true;
        cx.notice("Переключаемся на заместителя ведущего");
        Flow::Continue
    }

    /// Заместитель становится ведущим: при потере связи или по передаче управления.
    /// Мир, очки и счётчик ID наследуются из последнего снимка; старый MASTER
    /// удаляется. `graceful_order` задаёт версию первой публикации при штатном выходе.
    /// Не дошедшие до DEPUTY изменения здесь восстановить невозможно.
    fn become_master(&mut self, cx: &mut Ctx, graceful_order: Option<i32>) -> Flow {
        let Some(min_next) = self.order.checked_add(1) else {
            return Flow::exhausted_order();
        };
        let next = graceful_order
            .filter(|&n| n >= min_next)
            .unwrap_or(min_next);
        let old_addr = self.master_addr.take();
        if let Some(old_id) = self.master_id()
            && old_id != self.my_id
        {
            self.remove_player(old_id);
        }
        if let Some(a) = old_addr {
            self.rel.links.remove(&a);
        }
        self.role = NodeRole::Master;
        let my_id = self.my_id;
        if let Some(me) = self.players.iter_mut().find(|p| p.id == my_id) {
            me.role = NodeRole::Master;
            me.addr = None;
        }
        for p in self.players.iter().filter(|p| p.id != my_id) {
            if let Some(a) = p.addr {
                let l = self.rel.link(a, cx.now);
                l.peer_id = Some(p.id);
                l.ping = true;
                l.last_rx = cx.now;
                l.last_tx = cx.now;
            }
        }
        let delay = Duration::from_millis(self.cfg().state_delay_ms as u64);
        self.m = MasterState::new(cx.now, delay);
        self.failover_used = false;

        // publish сам прибавит единицу: первая версия нового MASTER должна
        // продолжить прежнюю нумерацию, а не начать её заново.
        self.order = next - 1;
        self.ensure_deputy();

        // Сначала уведомляем о смене ведущего, потом публикуем состояние.
        let targets: Vec<(SocketAddrV4, i32)> = self
            .players
            .iter()
            .filter(|p| p.id != my_id)
            .filter_map(|p| p.addr.map(|a| (a, p.id)))
            .collect();
        for (addr, id) in targets {
            let m = self.msg(
                Some(id),
                Body::RoleChange {
                    sender: Some(NodeRole::Master),
                    receiver: None,
                    order: next,
                },
            );
            self.rel.send_request(cx.ob, cx.now, addr, Kind::Control, m);
        }
        let flow = self.publish(cx);
        cx.notice("Вы стали ведущим игры");
        flow
    }

    // ─────────────────────────────── Выход ───────────────────────────────────

    /// Начинает выход. `Left` — игра для узла закончилась сразу.
    /// Обычный участник оставляет повторы Leave в `closing` и возвращается в лобби;
    /// MASTER остаётся в сессии до двух ACK передачи управления или дедлайна.
    fn begin_leave(&mut self, cx: &mut Ctx, closing: &mut Vec<Closing>) -> Flow {
        if self.role != NodeRole::Master {
            if let Some(to) = self.master_addr
                && let Some(seq) = cx.ob.seq()
            {
                let msg = self.msg(self.master_id(), Body::Leave);
                let msg = Message { seq, ..msg };
                cx.ob.unicast(to, &msg);
                closing.push(Closing {
                    to,
                    msg,
                    last_sent: cx.now,
                    deadline: cx.now + LEAVE_DEADLINE,
                });
            }
            return Flow::Left;
        }
        let target = self
            .deputy()
            .and_then(|d| d.addr.map(|a| (d.id, a)))
            .or_else(|| {
                self.players
                    .iter()
                    .find(|p| p.id != self.my_id && p.addr.is_some())
                    .map(|p| (p.id, p.addr.unwrap()))
            });
        let Some((id, addr)) = target else {
            return Flow::Left;
        };
        // Передача управления: актуальный State заместителю, затем смена роли.
        // Здесь State — Control: его нельзя вытеснить обычной рассылкой снимков,
        // ведь именно ACK на этот seq разрешает перейти к RoleChange.
        let state = self.build_state();
        let m = self.msg(Some(id), Body::State(state));
        if let Some(sent) = self.rel.send_request(cx.ob, cx.now, addr, Kind::Control, m) {
            self.leaving = Some(Leaving {
                stage: HandoverStage::State(sent.seq),
                deadline: cx.now + LEAVE_DEADLINE,
                to: addr,
            });
        }
        Flow::Continue
    }

    fn leave_tick(&mut self, cx: &mut Ctx) -> Flow {
        let dead = self.rel.tick(cx.ob, cx.now, &self.name, Some(self.my_id));
        let _ = dead;
        match &self.leaving {
            Some(l) if matches!(l.stage, HandoverStage::Complete) || cx.now >= l.deadline => {
                Flow::Left
            }
            _ => Flow::Continue,
        }
    }

    // ───────────────────────────── Диспетчеризация ───────────────────────────

    fn on_message(&mut self, cx: &mut Ctx, from: SocketAddrV4, msg: Message) -> Flow {
        if msg.game_name.as_deref() != Some(self.name.as_str()) {
            return Flow::Continue;
        }
        if self.leaving.is_some() {
            if matches!(msg.body, Body::Ack)
                && let Some(p) = self.rel.on_ack(cx.now, from, msg.seq)
            {
                return self.on_acked(cx, from, &p.msg);
            }
            return Flow::Continue;
        }
        if self.role == NodeRole::Master {
            self.m_message(cx, from, msg)
        } else {
            self.p_message(cx, from, msg)
        }
    }

    fn tick(&mut self, cx: &mut Ctx) -> Flow {
        if self.leaving.is_some() {
            return self.leave_tick(cx);
        }
        if self.role == NodeRole::Master {
            self.m_tick(cx)
        } else {
            self.p_tick(cx)
        }
    }

    fn steer(&mut self, cx: &mut Ctx, dir: Dir) {
        if self.status != PlayerStatus::Playing || self.leaving.is_some() {
            return;
        }
        if self.role == NodeRole::Master {
            if self.world.is_steer_valid(self.my_id, dir) {
                self.m.steers.insert(self.my_id, (0, dir));
            }
        } else if let Some(to) = self.master_addr {
            let m = self.msg(self.master_id(), Body::Steer(dir));
            self.rel.send_request(cx.ob, cx.now, to, Kind::Steer, m);
        }
    }

    fn toggle_watch(&mut self, cx: &mut Ctx) -> Flow {
        let target = match self.status {
            PlayerStatus::Playing => PlayerStatus::Viewing,
            PlayerStatus::Viewing => PlayerStatus::Playing,
        };
        if self.role == NodeRole::Master {
            match self.set_status(cx, self.my_id, target) {
                Ok(true) => return self.after_change(cx),
                Ok(false) => {}
                Err(why) => cx.ev.push(Event::Failure(why.to_owned())),
            }
        } else if let Some(to) = self.master_addr {
            let m = self.msg(self.master_id(), Body::StatusChange(target));
            self.rel.send_request(cx.ob, cx.now, to, Kind::Control, m);
        }
        Flow::Continue
    }
}

impl Phase {
    fn host(&mut self, cx: &mut Ctx, game: String, player: String, cfg: Config) {
        if !matches!(self, Phase::Lobby) {
            return;
        }
        match Game::new_master(cx, game, player, cfg) {
            Ok(game) => {
                cx.ev.push(Event::Joined);
                *self = Phase::InGame(Box::new(game));
            }
            Err(why) => cx.ev.push(Event::Failure(why)),
        }
    }

    fn host_on_server(
        &mut self,
        cx: &mut Ctx,
        server: Option<SocketAddrV4>,
        game: String,
        player: String,
    ) {
        if !matches!(self, Phase::Lobby) {
            return;
        }
        match server {
            Some(addr) if valid_game_name(&game) && valid_player_name(&player) => {
                let key = GameKey { name: game, addr };
                if let Some(joining) = Joining::start(cx, key, player, PlayerStatus::Playing, None)
                {
                    *self = Phase::Joining(Box::new(joining));
                }
            }
            Some(_) => cx
                .ev
                .push(Event::Failure("Недопустимое имя игры или игрока".into())),
            None => cx
                .ev
                .push(Event::Failure("Адрес сервера курса неизвестен".into())),
        }
    }

    fn join(
        &mut self,
        cx: &mut Ctx,
        catalog: &Catalog,
        key: GameKey,
        player: String,
        status: PlayerStatus,
    ) {
        if !matches!(self, Phase::Lobby) {
            return;
        }
        if !valid_player_name(&player) {
            cx.ev.push(Event::Failure("Недопустимое имя игрока".into()));
            return;
        }
        let cfg = catalog.get(&key).map(|entry| entry.config);
        if let Some(joining) = Joining::start(cx, key, player, status, cfg) {
            *self = Phase::Joining(Box::new(joining));
        }
    }

    fn steer(&mut self, cx: &mut Ctx, dir: Dir) {
        if let Phase::InGame(game) = self {
            game.steer(cx, dir);
        }
    }

    fn toggle_watch(&mut self, cx: &mut Ctx) -> Flow {
        match self {
            Phase::InGame(game) => game.toggle_watch(cx),
            _ => Flow::Continue,
        }
    }

    fn invite_robot(&self, cx: &mut Ctx, server: Option<SocketAddrV4>) {
        if let (Phase::InGame(game), Some(server)) = (self, server)
            && game.role == NodeRole::Master
            && game.leaving.is_none()
            && let Some(seq) = cx.ob.seq()
        {
            let msg = mk(
                seq,
                None,
                None,
                None,
                Body::Announcement(vec![game.announcement()]),
            );
            cx.ob.unicast(server, &msg);
            cx.notice("Приглашение отправлено серверу курса");
        }
    }

    fn begin_leave(&mut self, cx: &mut Ctx, closing: &mut Vec<Closing>) -> Flow {
        match self {
            Phase::InGame(game) if game.leaving.is_none() => game.begin_leave(cx, closing),
            Phase::Joining(_) => Flow::Left,
            _ => Flow::Continue,
        }
    }
}

// ───────────────────────────────── Узел ──────────────────────────────────────

/// Автомат одного локального узла: каталог, подключение и активная сессия.
///
/// Все входы вызываются последовательно с монотонным `now`. После них нужно
/// забирать исходящие пакеты и события через `drain_*`; собственных потоков,
/// сокетов или фоновых таймеров у узла нет.
pub struct Node {
    ob: Outbox,
    events: Vec<Event>,
    catalog: Catalog,
    rng: Rng,
    server: Option<SocketAddrV4>,
    server_seen: Option<Instant>,
    local_ips: Vec<Ipv4Addr>,
    my_port: u16,
    phase: Phase,
    closing: Vec<Closing>,
    last_discover: Option<Instant>,
    quitting: bool,
    now: Instant,
}

impl Node {
    /// `first_seq` — начальное значение `msg_seq`. Для нового сеанса на старом
    /// endpoint оно должно превышать всё, что отправлялось раньше.
    pub fn new(now: Instant, seed: u64, first_seq: i64) -> Node {
        Node {
            ob: Outbox::new(first_seq),
            events: Vec::new(),
            catalog: Catalog::new(),
            rng: Rng::new(seed),
            server: None,
            server_seen: None,
            local_ips: Vec::new(),
            my_port: 0,
            phase: Phase::Lobby,
            closing: Vec::new(),
            last_discover: None,
            quitting: false,
            now,
        }
    }

    pub fn set_trace(&mut self, on: bool) {
        self.ob.trace_on = on;
    }

    pub fn set_server(&mut self, addr: Option<SocketAddrV4>) {
        self.server = addr;
    }

    /// Локальные адреса и порт: по ним отбрасываются собственные multicast-анонсы.
    pub fn set_local(&mut self, ips: Vec<Ipv4Addr>, port: u16) {
        self.local_ips = ips;
        self.my_port = port;
    }

    /// Забирает накопленные датаграммы для транспорта, очищая очередь отправки.
    /// Запросы, ожидающие ACK, остаются в `Reliable` и при необходимости повторятся.
    pub fn drain_out(&mut self) -> Vec<Out> {
        std::mem::take(&mut self.ob.out)
    }

    /// Забирает накопленные уведомления для интерфейса, очищая очередь событий.
    pub fn drain_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }

    pub fn drain_trace(&mut self) -> Vec<String> {
        std::mem::take(&mut self.ob.trace)
    }

    pub fn phase(&self) -> PhaseKind {
        match self.phase {
            Phase::Lobby => PhaseKind::Lobby,
            Phase::Joining(_) => PhaseKind::Joining,
            Phase::InGame(_) => PhaseKind::InGame,
        }
    }

    pub fn catalog_entries(&self) -> Vec<&Entry> {
        self.catalog.list()
    }

    pub fn is_quitting(&self) -> bool {
        self.quitting
    }

    pub fn server_known(&self) -> bool {
        self.server.is_some()
    }

    /// Отвечал ли сервер курса в последние секунды.
    pub fn server_online(&self, now: Instant) -> bool {
        self.server_seen
            .is_some_and(|t| now.duration_since(t) <= Duration::from_millis(3000))
    }

    /// Читает текущий мир без копирования; вне активной игры возвращает `None`.
    pub fn game_view(&self) -> Option<GameView<'_>> {
        let Phase::InGame(g) = &self.phase else {
            return None;
        };
        Some(GameView {
            name: &g.name,
            cfg: g.cfg(),
            world: &g.world,
            players: &g.players,
            my_id: g.my_id,
            role: g.role,
            status: g.status,
            order: g.order,
            leaving: g.leaving.is_some(),
            master: g.master_addr,
            status_pending: g.rel.links.values().any(|l| {
                l.pending
                    .iter()
                    .any(|p| matches!(p.msg.body, Body::StatusChange(_)))
            }),
        })
    }

    pub fn joining_view(&self) -> Option<JoiningView<'_>> {
        let Phase::Joining(j) = &self.phase else {
            return None;
        };
        Some(j.view())
    }

    /// Только для тестов: изменяемый мир ведущего.
    #[doc(hidden)]
    pub fn debug_world_mut(&mut self) -> Option<&mut World> {
        match &mut self.phase {
            Phase::InGame(g) => Some(&mut g.world),
            _ => None,
        }
    }

    #[doc(hidden)]
    pub fn debug_role(&self) -> Option<NodeRole> {
        match &self.phase {
            Phase::InGame(g) => Some(g.role),
            _ => None,
        }
    }

    fn is_self(&self, from: SocketAddrV4) -> bool {
        self.my_port != 0 && from.port() == self.my_port && self.local_ips.contains(from.ip())
    }

    fn reject_exhausted_start(&mut self, cmd: &Command) -> bool {
        if self.ob.exhausted()
            && matches!(
                cmd,
                Command::Host { .. } | Command::HostOnServer { .. } | Command::Join { .. }
            )
        {
            self.events.push(Event::Failure(
                "Исчерпан msg_seq: для новой сессии нужен новый UDP endpoint".into(),
            ));
            true
        } else {
            false
        }
    }

    /// Обрабатывает пользовательское действие без ожидания сетевого ответа.
    /// Результат отражается в событиях и последующих представлениях состояния.
    pub fn command(&mut self, now: Instant, cmd: Command) {
        self.now = now;
        if self.reject_exhausted_start(&cmd) {
            return;
        }
        let Node {
            phase,
            ob,
            events,
            rng,
            closing,
            catalog,
            server,
            quitting,
            ..
        } = self;
        let mut cx = Ctx {
            now,
            ob,
            ev: events,
            rng,
        };
        let flow = match cmd {
            Command::Host { game, player, cfg } => {
                phase.host(&mut cx, game, player, cfg);
                Flow::Continue
            }
            Command::HostOnServer { game, player } => {
                phase.host_on_server(&mut cx, *server, game, player);
                Flow::Continue
            }
            Command::Join {
                key,
                player,
                status,
            } => {
                phase.join(&mut cx, catalog, key, player, status);
                Flow::Continue
            }
            Command::Steer(dir) => {
                phase.steer(&mut cx, dir);
                Flow::Continue
            }
            Command::ToggleWatch => phase.toggle_watch(&mut cx),
            Command::InviteRobot => {
                phase.invite_robot(&mut cx, *server);
                Flow::Continue
            }
            Command::Leave => phase.begin_leave(&mut cx, closing),
            Command::Quit => {
                *quitting = true;
                phase.begin_leave(&mut cx, closing)
            }
        };
        self.finish_flow(flow);
        self.after_call();
    }

    /// Точка входа пакета от транспорта: декодирование, обнаружение игр, затем
    /// диспетчеризация по фазе. `via_multicast` указывает сокет приёма, не адрес
    /// отправителя; проверку предельного размера датаграммы выполняет транспорт.
    pub fn on_datagram(
        &mut self,
        now: Instant,
        from: SocketAddrV4,
        via_multicast: bool,
        bytes: &[u8],
    ) {
        self.now = now;
        let msg = match decode(bytes) {
            Ok(m) => m,
            Err(e) => {
                self.ob
                    .note(|| format!("RX {from} отброшено: {} ({} байт)", e.0, bytes.len()));
                return;
            }
        };
        self.ob.note(|| {
            format!(
                "RX {from}{} {}",
                if via_multicast { " [mc]" } else { "" },
                describe(&msg)
            )
        });
        if Some(from) == self.server && !via_multicast {
            self.server_seen = Some(now);
        }

        if matches!(msg.body, Body::Ack) {
            self.closing
                .retain(|c| !(c.to == from && c.msg.seq == msg.seq));
        }

        match &msg.body {
            Body::Announcement(games) => {
                if !self.is_self(from) {
                    self.on_announcement(now, from, via_multicast, games);
                }
                self.after_call();
                return;
            }
            Body::Discover => {
                if let Phase::InGame(g) = &self.phase
                    && g.role == NodeRole::Master
                    && g.leaving.is_none()
                    && let Some(seq) = self.ob.seq()
                {
                    let m = mk(
                        seq,
                        None,
                        None,
                        None,
                        Body::Announcement(vec![g.announcement()]),
                    );
                    self.ob.unicast(from, &m);
                }
                self.after_call();
                return;
            }
            _ => {}
        }

        let Node {
            phase,
            ob,
            events,
            rng,
            ..
        } = self;
        let mut cx = Ctx {
            now,
            ob,
            ev: events,
            rng,
        };
        let mut flow = Flow::Continue;
        let mut join_step = JoinStep::Wait;
        match phase {
            Phase::Lobby => {}
            Phase::Joining(j) => join_step = j.on_message(&mut cx, from, msg),
            Phase::InGame(g) => flow = g.on_message(&mut cx, from, msg),
        }
        self.finish_join(join_step);
        self.finish_flow(flow);
        self.after_call();
    }

    fn on_announcement(
        &mut self,
        now: Instant,
        from: SocketAddrV4,
        via_multicast: bool,
        games: &[Announcement],
    ) {
        // Multicast всегда считается локальной сетью; unicast с адреса сервера — сервером курса.
        let source = if !via_multicast && Some(from) == self.server {
            Source::Server
        } else {
            Source::Lan
        };
        self.catalog.ingest(now, from, games, source);

        let Node {
            phase,
            ob,
            events,
            rng,
            ..
        } = self;
        let mut cx = Ctx {
            now,
            ob,
            ev: events,
            rng,
        };
        let step = match phase {
            Phase::Joining(j) => j.on_announcement(&mut cx, from, games),
            _ => JoinStep::Wait,
        };
        self.finish_join(step);
    }

    fn finish_join(&mut self, step: JoinStep) {
        match step {
            JoinStep::Wait => {}
            JoinStep::Done(g) => {
                self.phase = Phase::InGame(g);
                self.events.push(Event::Joined);
            }
            JoinStep::Failed(why) => {
                self.phase = Phase::Lobby;
                self.events.push(Event::Failure(why));
            }
        }
    }

    /// Продвигает таймеры без ожидания: TTL каталога, Discover, повторы, Ping,
    /// тайм-ауты, игровые ходы и завершение выхода. Вызывается чаще игрового хода,
    /// иначе медленная игра задерживала бы обслуживание связи.
    pub fn on_tick(&mut self, now: Instant) {
        self.now = now;
        self.catalog.expire(now);

        let Node {
            phase,
            ob,
            events,
            rng,
            last_discover,
            server,
            ..
        } = self;
        let mut cx = Ctx {
            now,
            ob,
            ev: events,
            rng,
        };
        let mut flow = Flow::Continue;
        let mut join_step = JoinStep::Wait;
        match phase {
            Phase::Lobby
                if let Some(srv) = *server
                    && last_discover.is_none_or(|t| now.duration_since(t) >= DISCOVER_PERIOD)
                    && !cx.ob.exhausted()
                    && let Some(seq) = cx.ob.seq() =>
            {
                let m = mk(seq, None, None, None, Body::Discover);
                cx.ob.unicast(srv, &m);
                *last_discover = Some(now);
            }
            Phase::Lobby => {}
            Phase::Joining(j) => join_step = j.tick(&mut cx, *server),
            Phase::InGame(g) => flow = g.tick(&mut cx),
        }
        self.finish_join(join_step);

        // Повторы сообщений о выходе.
        for c in &mut self.closing {
            if now.duration_since(c.last_sent) >= crate::reliable::RESEND_AFTER {
                self.ob.unicast(c.to, &c.msg);
                c.last_sent = now;
            }
        }
        self.closing.retain(|c| now < c.deadline);

        self.finish_flow(flow);
        self.after_call();
    }

    fn finish_flow(&mut self, flow: Flow) {
        match flow {
            Flow::Continue => {}
            Flow::Ended(why) => {
                self.phase = Phase::Lobby;
                self.catalog.clear();
                self.events.push(Event::GameOver(why));
            }
            Flow::Failed(why) => {
                self.phase = Phase::Lobby;
                self.catalog.clear();
                self.events.push(Event::Failure(why.clone()));
                self.events.push(Event::GameOver(why));
            }
            Flow::Left => {
                self.phase = Phase::Lobby;
                self.catalog.clear();
                self.events.push(Event::GameOver("Вы вышли из игры".into()));
            }
        }
    }

    /// Общие проверки после любого вызова: ошибка реализации, завершение работы.
    fn after_call(&mut self) {
        match self.ob.fault.take() {
            Some(Fault::SequenceExhausted) => {
                let in_game = matches!(self.phase, Phase::InGame(_));
                self.phase = Phase::Lobby;
                self.closing.clear();
                let why = "Исчерпан msg_seq: для новой сессии нужен новый UDP endpoint".to_owned();
                self.events.push(Event::Failure(why.clone()));
                if in_game {
                    self.events.push(Event::GameOver(why));
                }
            }
            Some(Fault::Encoding(why)) => {
                self.phase = Phase::Lobby;
                self.events
                    .push(Event::Failure(format!("Внутренняя ошибка: {why}")));
            }
            None => {}
        }
        if self.quitting && matches!(self.phase, Phase::Lobby) && self.closing.is_empty() {
            self.quitting = false;
            self.events.push(Event::Exited);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host_at_order(now: Instant, order: i32) -> Node {
        let mut node = Node::new(now, 1, 1);
        node.command(
            now,
            Command::Host {
                game: "g".into(),
                player: "host".into(),
                cfg: Config::default(),
            },
        );
        let Phase::InGame(game) = &mut node.phase else {
            panic!("игра не создана");
        };
        game.order = order;
        node.drain_events();
        node
    }

    fn join_viewer(node: &mut Node, now: Instant, addr: SocketAddrV4) {
        let join = mk(
            1,
            None,
            None,
            Some("g"),
            Body::Join {
                ptype: PlayerType::Human,
                player_name: "deputy".into(),
                game_name: "g".into(),
                status: PlayerStatus::Viewing,
            },
        );
        node.on_datagram(now, addr, false, &encode(&join).unwrap());
        node.drain_out();
        node.drain_events();
    }

    #[test]
    fn invalid_lobby_commands_report_failures_without_starting_a_session() {
        let now = Instant::now();
        let mut node = Node::new(now, 1, 1);
        node.command(
            now,
            Command::Host {
                game: "".into(),
                player: "host".into(),
                cfg: Config::default(),
            },
        );
        node.command(
            now,
            Command::HostOnServer {
                game: "g".into(),
                player: "host".into(),
            },
        );
        node.set_server(Some(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 9193)));
        node.command(
            now,
            Command::HostOnServer {
                game: "".into(),
                player: "host".into(),
            },
        );
        node.command(
            now,
            Command::Join {
                key: GameKey {
                    name: "g".into(),
                    addr: SocketAddrV4::new(Ipv4Addr::LOCALHOST, 4000),
                },
                player: "".into(),
                status: PlayerStatus::Viewing,
            },
        );
        assert_eq!(node.phase(), PhaseKind::Lobby);
        assert!(node.drain_out().is_empty());
        let failures = node
            .drain_events()
            .into_iter()
            .filter(|e| matches!(e, Event::Failure(_)))
            .count();
        assert_eq!(failures, 4);
    }

    #[test]
    fn inviting_the_server_is_allowed_only_for_an_active_master() {
        let now = Instant::now();
        let server = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 9193);
        let mut node = Node::new(now, 1, 1);
        node.set_server(Some(server));
        node.command(now, Command::InviteRobot);
        assert!(node.drain_out().is_empty());
        node.command(
            now,
            Command::Host {
                game: "g".into(),
                player: "host".into(),
                cfg: Config::default(),
            },
        );
        node.command(now, Command::InviteRobot);
        let packets = node.drain_out();
        assert!(packets.iter().any(|out| match out {
            Out::Unicast(to, bytes) if *to == server => matches!(decode(bytes).unwrap().body, Body::Announcement(games) if games.len() == 1 && games[0].game_name == "g"),
            _ => false,
        }));
        assert!(
            node.drain_events()
                .iter()
                .any(|e| matches!(e, Event::Notice(reason) if reason.contains("Приглашение")))
        );
        node.command(now, Command::Leave);
        node.command(now, Command::InviteRobot);
        assert!(node.drain_out().is_empty());
    }

    #[test]
    fn quitting_while_joining_returns_to_lobby_and_emits_exit() {
        let now = Instant::now();
        let mut node = Node::new(now, 1, 1);
        node.command(
            now,
            Command::Join {
                key: GameKey {
                    name: "g".into(),
                    addr: SocketAddrV4::new(Ipv4Addr::LOCALHOST, 4000),
                },
                player: "player".into(),
                status: PlayerStatus::Viewing,
            },
        );
        assert_eq!(node.phase(), PhaseKind::Joining);
        node.command(now, Command::Quit);
        assert_eq!(node.phase(), PhaseKind::Lobby);
        assert!(node.drain_events().contains(&Event::Exited));
    }

    #[test]
    fn first_status_request_after_zero_join_is_not_treated_as_stale() {
        let now = Instant::now();
        let mut node = host_at_order(now, 0);
        let peer = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 4000);
        let join = mk(
            0,
            None,
            None,
            Some("g"),
            Body::Join {
                ptype: PlayerType::Human,
                player_name: "viewer".into(),
                game_name: "g".into(),
                status: PlayerStatus::Viewing,
            },
        );
        node.on_datagram(now, peer, false, &encode(&join).unwrap());
        let change = mk(
            1,
            Some(2),
            Some(1),
            Some("g"),
            Body::StatusChange(PlayerStatus::Playing),
        );
        node.on_datagram(now, peer, false, &encode(&change).unwrap());
        assert_eq!(
            node.game_view()
                .unwrap()
                .players
                .iter()
                .find(|p| p.id == 2)
                .unwrap()
                .status,
            PlayerStatus::Playing
        );
    }

    #[test]
    fn first_steer_after_zero_join_is_applied() {
        let now = Instant::now();
        let mut node = host_at_order(now, 0);
        let peer = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 4000);
        let join = mk(
            0,
            None,
            None,
            Some("g"),
            Body::Join {
                ptype: PlayerType::Human,
                player_name: "player".into(),
                game_name: "g".into(),
                status: PlayerStatus::Playing,
            },
        );
        node.on_datagram(now, peer, false, &encode(&join).unwrap());
        let world = node.debug_world_mut().unwrap();
        for snake in &mut world.snakes {
            let head = if snake.owner == 2 { (10, 7) } else { (1, 1) };
            snake.cells = [head, (head.0 - 1, head.1)].into_iter().collect();
            snake.dir = Dir::Right;
        }
        world.foods.clear();
        let steer = mk(1, Some(2), Some(1), Some("g"), Body::Steer(Dir::Up));
        node.on_datagram(now, peer, false, &encode(&steer).unwrap());
        let ping = mk(2, Some(2), Some(1), Some("g"), Body::Ping);
        node.on_datagram(
            now + Duration::from_millis(500),
            peer,
            false,
            &encode(&ping).unwrap(),
        );
        node.on_tick(now + Duration::from_secs(1));
        assert_eq!(
            node.game_view().unwrap().world.snake(2).unwrap().dir,
            Dir::Up
        );
    }

    #[test]
    fn master_rejects_each_wrong_participant_id_without_advancing_the_request_window() {
        let now = Instant::now();
        let mut node = host_at_order(now, 0);
        let peer = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 4000);
        join_viewer(&mut node, now, peer);
        for (seq, sender, receiver) in [
            (20, Some(9), Some(1)),
            (21, Some(2), Some(9)),
            (22, None, Some(1)),
            (23, Some(2), None),
        ] {
            let request = mk(
                seq,
                sender,
                receiver,
                Some("g"),
                Body::StatusChange(PlayerStatus::Playing),
            );
            node.on_datagram(now, peer, false, &encode(&request).unwrap());
        }
        assert!(node.drain_out().is_empty());
        assert_eq!(
            node.game_view()
                .unwrap()
                .players
                .iter()
                .find(|p| p.id == 2)
                .unwrap()
                .status,
            PlayerStatus::Viewing
        );
        let valid = mk(
            20,
            Some(2),
            Some(1),
            Some("g"),
            Body::StatusChange(PlayerStatus::Playing),
        );
        node.on_datagram(now, peer, false, &encode(&valid).unwrap());
        assert_eq!(
            node.game_view()
                .unwrap()
                .players
                .iter()
                .find(|p| p.id == 2)
                .unwrap()
                .status,
            PlayerStatus::Playing
        );
        assert!(node.drain_out().iter().any(|out| match out {
            Out::Unicast(to, bytes) if *to == peer =>
                matches!(decode(bytes).unwrap().body, Body::Ack),
            _ => false,
        }));
    }

    #[test]
    fn master_keeps_separate_ordering_for_status_and_steering() {
        let now = Instant::now();
        let mut node = host_at_order(now, 0);
        let peer = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 4000);
        join_viewer(&mut node, now, peer);
        let send = |node: &mut Node, when, seq, body| {
            let message = mk(seq, Some(2), Some(1), Some("g"), body);
            node.on_datagram(when, peer, false, &encode(&message).unwrap());
        };
        send(
            &mut node,
            now,
            20,
            Body::StatusChange(PlayerStatus::Playing),
        );
        send(
            &mut node,
            now,
            19,
            Body::StatusChange(PlayerStatus::Viewing),
        );
        assert_eq!(
            node.game_view()
                .unwrap()
                .players
                .iter()
                .find(|p| p.id == 2)
                .unwrap()
                .status,
            PlayerStatus::Playing
        );
        let world = node.debug_world_mut().unwrap();
        for snake in &mut world.snakes {
            let head = if snake.owner == 2 { (10, 7) } else { (1, 1) };
            snake.cells = [head, (head.0 - 1, head.1)].into_iter().collect();
            snake.dir = Dir::Right;
        }
        world.foods.clear();
        send(&mut node, now, 30, Body::Steer(Dir::Up));
        send(&mut node, now, 31, Body::Steer(Dir::Left));
        send(&mut node, now, 29, Body::Steer(Dir::Down));
        let replies = node.drain_out();
        for seq in [20, 19, 30, 31, 29] {
            assert!(
                replies.iter().any(|out| match out {
                    Out::Unicast(to, bytes) if *to == peer => {
                        let message = decode(bytes).unwrap();
                        message.seq == seq && matches!(message.body, Body::Ack)
                    }
                    _ => false,
                }),
                "нет ACK для {seq}"
            );
        }
        send(&mut node, now + Duration::from_millis(500), 32, Body::Ping);
        let next = now + Duration::from_secs(1);
        node.on_tick(next);
        assert_eq!(
            node.game_view().unwrap().world.snake(2).unwrap().dir,
            Dir::Up
        );
        send(
            &mut node,
            next,
            21,
            Body::StatusChange(PlayerStatus::Viewing),
        );
        assert_eq!(
            node.game_view()
                .unwrap()
                .players
                .iter()
                .find(|p| p.id == 2)
                .unwrap()
                .status,
            PlayerStatus::Viewing
        );
    }

    #[test]
    fn requesting_the_current_status_returns_the_current_state_without_a_new_version() {
        let now = Instant::now();
        let mut node = host_at_order(now, 0);
        let viewer = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 4000);
        join_viewer(&mut node, now, viewer);
        let order = node.game_view().unwrap().order;
        let request = mk(
            2,
            Some(2),
            Some(1),
            Some("g"),
            Body::StatusChange(PlayerStatus::Viewing),
        );
        node.on_datagram(now, viewer, false, &encode(&request).unwrap());
        let packets: Vec<_> = node
            .drain_out()
            .into_iter()
            .filter_map(|out| match out {
                Out::Unicast(to, bytes) if to == viewer => Some(decode(&bytes).unwrap()),
                _ => None,
            })
            .collect();
        assert!(
            packets
                .iter()
                .any(|m| m.seq == request.seq && matches!(m.body, Body::Ack))
        );
        assert!(packets.iter().any(|m| matches!(&m.body, Body::State(state) if state.order == order && state.players.iter().any(|p| p.id == 2 && p.status == PlayerStatus::Viewing))));
        assert_eq!(node.game_view().unwrap().order, order);
    }

    #[test]
    fn state_order_ends_game_instead_of_publishing_the_same_version_twice() {
        let now = Instant::now();
        let mut node = host_at_order(now, 0);
        let deputy = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 4000);
        join_viewer(&mut node, now, deputy);
        let Phase::InGame(game) = &mut node.phase else {
            unreachable!();
        };
        game.order = i32::MAX - 1;
        node.command(now, Command::ToggleWatch);
        assert_eq!(node.game_view().unwrap().order, i32::MAX);
        assert!(node.drain_out().iter().any(|out| match out {
            Out::Unicast(to, bytes) if *to == deputy => {
                matches!(decode(bytes).unwrap().body, Body::State(state) if state.order == i32::MAX)
            }
            _ => false,
        }));

        node.command(now, Command::ToggleWatch);
        assert!(node.drain_out().is_empty());
        assert_eq!(node.phase(), PhaseKind::Lobby);
        assert!(node.drain_events().iter().any(
            |event| matches!(event, Event::GameOver(reason) if reason.contains("state_order"))
        ));
    }

    fn deputy_at_order(now: Instant, order: i32) -> Node {
        let mut node = host_at_order(now, order);
        let master = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 4000);
        let Phase::InGame(game) = &mut node.phase else {
            unreachable!();
        };
        game.role = NodeRole::Deputy;
        game.players[0].role = NodeRole::Deputy;
        game.players.push(PlayerInfo {
            name: "master".into(),
            id: 2,
            addr: Some(master),
            role: NodeRole::Master,
            ptype: PlayerType::Human,
            score: 0,
            status: PlayerStatus::Viewing,
        });
        game.master_addr = Some(master);
        let link = game.rel.link(master, now);
        link.peer_id = Some(2);
        link.ping = true;
        node
    }

    #[test]
    fn deputy_can_use_the_last_order_after_failover() {
        let now = Instant::now();
        let mut node = deputy_at_order(now, i32::MAX - 1);
        node.on_tick(now + Duration::from_millis(1001));
        assert_eq!(node.game_view().unwrap().role, NodeRole::Master);
        assert_eq!(node.game_view().unwrap().order, i32::MAX);
        node.on_tick(now + Duration::from_millis(2001));
        assert_eq!(node.phase(), PhaseKind::Lobby);
        assert!(node.drain_events().iter().any(
            |event| matches!(event, Event::GameOver(reason) if reason.contains("state_order"))
        ));
    }

    #[test]
    fn deputy_does_not_publish_a_reused_order_on_failover() {
        let now = Instant::now();
        let mut node = deputy_at_order(now, i32::MAX);
        node.on_tick(now + Duration::from_millis(1001));
        assert_eq!(node.phase(), PhaseKind::Lobby);
        assert!(node.drain_events().iter().any(
            |event| matches!(event, Event::GameOver(reason) if reason.contains("state_order"))
        ));
        assert!(!node.drain_out().iter().any(|out| match out {
            Out::Unicast(_, bytes) | Out::Multicast(bytes) => {
                matches!(
                    decode(bytes).unwrap().body,
                    Body::State(_) | Body::RoleChange { .. }
                )
            }
        }));
    }

    #[test]
    fn master_cannot_handover_with_an_exhausted_state_order() {
        let now = Instant::now();
        let mut node = host_at_order(now, 0);
        let deputy = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 4000);
        join_viewer(&mut node, now, deputy);
        let Phase::InGame(game) = &mut node.phase else {
            unreachable!();
        };
        game.order = i32::MAX;
        node.command(now, Command::Leave);
        let state = node
            .drain_out()
            .into_iter()
            .find_map(|out| match out {
                Out::Unicast(_, bytes) => {
                    let message = decode(&bytes).unwrap();
                    matches!(message.body, Body::State(_)).then_some(message)
                }
                _ => None,
            })
            .unwrap();
        let ack = mk(state.seq, Some(2), Some(1), Some("g"), Body::Ack);
        node.on_datagram(now, deputy, false, &encode(&ack).unwrap());
        assert_eq!(node.phase(), PhaseKind::Lobby);
        assert!(node.drain_events().iter().any(
            |event| matches!(event, Event::GameOver(reason) if reason.contains("state_order"))
        ));
        assert!(!node.drain_out().iter().any(|out| match out {
            Out::Unicast(_, bytes) | Out::Multicast(bytes) => {
                matches!(decode(bytes).unwrap().body, Body::RoleChange { .. })
            }
        }));
    }

    #[test]
    fn sequence_exhaustion_ends_the_session_without_reusing_a_number() {
        let now = Instant::now();
        let mut node = Node::new(now, 1, i64::MAX - 1);
        node.command(
            now,
            Command::Host {
                game: "g".into(),
                player: "host".into(),
                cfg: Config::default(),
            },
        );
        node.on_tick(now);
        let Out::Multicast(bytes) = node.drain_out().pop().unwrap() else {
            panic!("ожидался анонс");
        };
        assert_eq!(decode(&bytes).unwrap().seq, i64::MAX - 1);

        let from = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 4000);
        let discover = encode(&mk(1, None, None, None, Body::Discover)).unwrap();
        node.on_datagram(now, from, false, &discover);
        let Out::Unicast(_, bytes) = node.drain_out().pop().unwrap() else {
            panic!("ожидался ответ на Discover");
        };
        assert_eq!(decode(&bytes).unwrap().seq, i64::MAX);
        assert_eq!(node.phase(), PhaseKind::Lobby);
        assert!(
            node.drain_events()
                .iter()
                .any(|event| matches!(event, Event::Failure(reason) if reason.contains("msg_seq")))
        );

        node.on_datagram(now, from, false, &discover);
        node.set_server(Some(from));
        node.on_tick(now + Duration::from_secs(1));
        assert!(node.drain_out().is_empty());
        assert!(node.drain_events().is_empty());

        node.command(
            now + Duration::from_secs(1),
            Command::Host {
                game: "new".into(),
                player: "host".into(),
                cfg: Config::default(),
            },
        );
        assert_eq!(node.phase(), PhaseKind::Lobby);
        assert!(
            node.drain_events().iter().any(
                |event| matches!(event, Event::Failure(reason) if reason.contains("endpoint"))
            )
        );
    }
}
