//! Регистрация участника: ожидание ACK, конфигурации и снимка в любом порядке.

use std::net::SocketAddrV4;
use std::time::{Duration, Instant};

use crate::catalog::GameKey;
use crate::reliable::{Kind, Reliable};
use crate::wire::{
    Announcement, Body, Config, Message, NodeRole, PlayerStatus, PlayerType, StateMsg,
};

use super::{
    Ctx, DISCOVER_PERIOD, Game, JOIN_DEADLINE, JOIN_DISCOVER_PERIOD, JoiningView, MasterState,
    accept_request, mk, validate_state,
};

/// Независимо собирает подтверждение регистрации, конфиг и самый новый State.
/// Отсутствие конфига не мешает подтверждать пакеты и повторять Join.
pub(super) struct Joining {
    key: GameKey,
    player: String,
    status: PlayerStatus,
    stage: Stage,
    started: Instant,
    join_seq: i64,
    cfg: Option<Config>,
    acked: bool,
    my_id: Option<i32>,
    early_id: Option<i32>,
    master_id: Option<i32>,
    buffered: Option<(StateMsg, Option<i32>)>,
    last_discover: Instant,
    last_search_discover: Option<Instant>,
    rel: Reliable,
}

const SEARCH_DEADLINE: Duration = Duration::from_secs(3);

enum Stage {
    Attempt,
    Searching { since: Instant, reason: String },
}

pub(super) enum JoinStep {
    Wait,
    Done(Box<Game>),
    Failed(String),
}

impl Joining {
    pub(super) fn start(
        cx: &mut Ctx,
        key: GameKey,
        player: String,
        status: PlayerStatus,
        cfg: Option<Config>,
    ) -> Option<Joining> {
        let mut rel = Reliable::new();
        let join = mk(
            0,
            None,
            None,
            Some(&key.name),
            Body::Join {
                ptype: PlayerType::Human,
                player_name: player.clone(),
                game_name: key.name.clone(),
                status,
            },
        );
        let sent = rel.send_request(cx.ob, cx.now, key.addr, Kind::Control, join)?;
        Some(Joining {
            key,
            player,
            status,
            stage: Stage::Attempt,
            started: cx.now,
            join_seq: sent.seq,
            cfg,
            acked: false,
            my_id: None,
            early_id: None,
            master_id: None,
            buffered: None,
            last_discover: cx.now,
            last_search_discover: None,
            rel,
        })
    }

    pub(super) fn view(&self) -> JoiningView<'_> {
        let (started, recovering) = match self.stage {
            Stage::Attempt => (self.started, false),
            Stage::Searching { since, .. } => (since, true),
        };
        JoiningView {
            game: &self.key.name,
            started,
            recovering,
            acked: !recovering && self.acked,
            has_config: !recovering && self.cfg.is_some(),
        }
    }

    pub(super) fn on_announcement(
        &mut self,
        cx: &mut Ctx,
        from: SocketAddrV4,
        games: &[Announcement],
    ) -> JoinStep {
        if let Some(step) = self.expire(cx) {
            return step;
        }
        let Some(game) = games
            .iter()
            .find(|g| g.game_name == self.key.name && g.config.is_valid())
        else {
            return JoinStep::Wait;
        };
        match self.stage {
            Stage::Attempt if from == self.key.addr => {
                self.cfg = Some(game.config);
                self.try_complete(cx)
            }
            Stage::Searching { .. } if from != self.key.addr => {
                let key = GameKey {
                    name: self.key.name.clone(),
                    addr: from,
                };
                if let Some(joining) =
                    Self::start(cx, key, self.player.clone(), self.status, Some(game.config))
                {
                    *self = joining;
                    cx.notice("Найден новый ведущий; повторяем подключение");
                }
                JoinStep::Wait
            }
            _ => JoinStep::Wait,
        }
    }

    fn search(&mut self, cx: &mut Ctx) {
        self.stage = Stage::Searching {
            since: cx.now,
            reason: self.failure_reason(),
        };
        self.rel = Reliable::new();
        self.buffered = None;
        self.last_search_discover = None;
        cx.notice("Ведущий не отвечает; ищем новый анонс игры");
    }

    fn expire(&mut self, cx: &mut Ctx) -> Option<JoinStep> {
        match &self.stage {
            Stage::Searching { since, reason }
                if cx.now.duration_since(*since) >= SEARCH_DEADLINE =>
            {
                Some(JoinStep::Failed(format!(
                    "{reason}; новый анонс игры не найден за 3 секунды"
                )))
            }
            Stage::Attempt if cx.now.duration_since(self.started) >= JOIN_DEADLINE => {
                if self.acked {
                    Some(JoinStep::Failed(self.failure_reason()))
                } else {
                    self.search(cx);
                    Some(JoinStep::Wait)
                }
            }
            _ => None,
        }
    }

    fn discover_candidates(&mut self, cx: &mut Ctx, server: Option<SocketAddrV4>) {
        if let Some(server) = server
            && self
                .last_search_discover
                .is_none_or(|last| cx.now.duration_since(last) >= DISCOVER_PERIOD)
            && let Some(seq) = cx.ob.seq()
        {
            cx.ob
                .unicast(server, &mk(seq, None, None, None, Body::Discover));
            self.last_search_discover = Some(cx.now);
        }
    }

    fn failure_reason(&self) -> String {
        let why = if !self.acked {
            "нет ответа на запрос присоединения (ACK не получен)"
        } else if self.cfg.is_none() {
            "не получена конфигурация игры"
        } else {
            "не получено состояние игры"
        };
        format!("Не удалось подключиться к «{}»: {why}", self.key.name)
    }

    pub(super) fn on_message(
        &mut self,
        cx: &mut Ctx,
        from: SocketAddrV4,
        msg: Message,
    ) -> JoinStep {
        if let Some(step) = self.expire(cx) {
            return step;
        }
        if !matches!(self.stage, Stage::Attempt)
            || !self.matches_message(from, &msg)
            || !self.matches_ids(&msg)
        {
            return JoinStep::Wait;
        }
        if matches!(
            msg.body,
            Body::State(_) | Body::Ping | Body::RoleChange { .. } | Body::Error { .. }
        ) {
            self.rel.touch(from, cx.now);
        }
        self.handle_message(cx, from, msg)
    }

    fn matches_ids(&self, msg: &Message) -> bool {
        if !self.acked && matches!(msg.body, Body::Ack) {
            return true;
        }
        self.master_id
            .zip(msg.sender_id)
            .is_none_or(|(expected, actual)| expected == actual)
            && self
                .my_id
                .or(self.early_id)
                .zip(msg.receiver_id)
                .is_none_or(|(expected, actual)| expected == actual)
    }

    fn handle_message(&mut self, cx: &mut Ctx, from: SocketAddrV4, msg: Message) -> JoinStep {
        let name = self.key.name.clone();
        let my_id = self.my_id.or(self.early_id);
        match msg.body {
            Body::Ack => {
                if let Some(why) = self.on_ack(cx, from, &msg) {
                    return JoinStep::Failed(why);
                }
            }
            Body::Error {
                ref text,
                request_seq,
            } => {
                accept_request(&mut self.rel, cx, from, &msg, my_id, self.master_id, &name);
                if request_seq == self.join_seq {
                    return JoinStep::Failed(format!("Ведущий отклонил присоединение: {text}"));
                }
            }
            Body::State(ref s) => self.on_state(cx, from, &msg, s),
            Body::Ping | Body::RoleChange { .. } => {
                accept_request(
                    &mut self.rel,
                    cx,
                    from,
                    &msg,
                    my_id.or(msg.receiver_id),
                    self.master_id.or(msg.sender_id),
                    &name,
                );
            }
            _ => {}
        }
        self.try_complete(cx)
    }

    fn matches_message(&self, from: SocketAddrV4, msg: &Message) -> bool {
        from == self.key.addr && msg.game_name.as_deref() == Some(self.key.name.as_str())
    }

    fn on_ack(&mut self, cx: &mut Ctx, from: SocketAddrV4, msg: &Message) -> Option<String> {
        if let Some(p) = self.rel.on_ack(cx.now, from, msg.seq)
            && matches!(p.msg.body, Body::Join { .. })
        {
            if let (Some(early), Some(given)) = (self.early_id, msg.receiver_id)
                && early != given
            {
                return Some("Ведущий выдал противоречивые ID участника".into());
            }
            self.acked = true;
            self.my_id = msg.receiver_id.or(self.early_id);
            self.master_id = msg.sender_id;
            let link = self.rel.link(from, cx.now);
            link.peer_id = self.master_id;
            link.ping = true;
        }
        None
    }

    fn on_state(&mut self, cx: &mut Ctx, from: SocketAddrV4, msg: &Message, state: &StateMsg) {
        let my_id = self.my_id.or(self.early_id).or(msg.receiver_id);
        if accept_request(
            &mut self.rel,
            cx,
            from,
            msg,
            my_id,
            self.master_id.or(msg.sender_id),
            &self.key.name,
        ) {
            self.early_id = self.early_id.or(msg.receiver_id);
            let newer = self
                .buffered
                .as_ref()
                .is_none_or(|(b, _)| state.order > b.order);
            if newer {
                self.buffered = Some((state.clone(), msg.receiver_id));
            }
        }
    }

    /// Переносит Reliable вместе с ответами на ранние пакеты и неподтверждёнными запросами.
    fn try_complete(&mut self, cx: &mut Ctx) -> JoinStep {
        if !self.acked {
            return JoinStep::Wait;
        }
        let Some(cfg) = self.cfg else {
            return JoinStep::Wait;
        };
        let Some((state, rid)) = self.buffered.take() else {
            return JoinStep::Wait;
        };
        let Some(my_id) = self.my_id.or(rid) else {
            self.buffered = Some((state, rid));
            return JoinStep::Wait;
        };
        let (world, players, next_id) = match validate_state(cfg, &state) {
            Ok(v) => v,
            Err(why) => {
                return JoinStep::Failed(format!("Получено некорректное состояние игры: {why}"));
            }
        };
        let Some(me) = players.iter().find(|p| p.id == my_id).cloned() else {
            return JoinStep::Failed("В состоянии игры нет вашего участника".into());
        };
        if me.role == NodeRole::Master {
            return JoinStep::Failed("Ведущий указал вас как ведущего".into());
        }
        let master_id = players
            .iter()
            .find(|p| p.role == NodeRole::Master)
            .map(|p| p.id);
        let mut rel = std::mem::take(&mut self.rel);
        let link = rel.link(self.key.addr, cx.now);
        link.peer_id = master_id;
        link.ping = true;
        let delay = Duration::from_millis(cfg.state_delay_ms as u64);
        JoinStep::Done(Box::new(Game {
            name: self.key.name.clone(),
            my_id,
            role: me.role,
            status: me.status,
            world,
            players,
            order: state.order,
            next_id,
            master_addr: Some(self.key.addr),
            failover_used: false,
            rel,
            m: MasterState::new(cx.now, delay),
            leaving: None,
        }))
    }

    pub(super) fn tick(&mut self, cx: &mut Ctx, server: Option<SocketAddrV4>) -> JoinStep {
        if matches!(self.stage, Stage::Searching { .. }) {
            if let Some(step) = self.expire(cx) {
                return step;
            }
            self.discover_candidates(cx, server);
            return JoinStep::Wait;
        }
        let dead = self.rel.tick(cx.ob, cx.now, &self.key.name, self.my_id);
        if self.acked && dead.contains(&self.key.addr) {
            self.search(cx);
            self.discover_candidates(cx, server);
            return JoinStep::Wait;
        }
        if let Some(step) = self.expire(cx) {
            if matches!(self.stage, Stage::Searching { .. }) {
                self.discover_candidates(cx, server);
            }
            return step;
        }
        if self.cfg.is_none()
            && self.acked
            && cx.now.duration_since(self.last_discover) >= JOIN_DISCOVER_PERIOD
            && let Some(seq) = cx.ob.seq()
        {
            let m = mk(seq, None, None, None, Body::Discover);
            cx.ob.unicast(self.key.addr, &m);
            self.last_discover = cx.now;
        }
        JoinStep::Wait
    }
}
