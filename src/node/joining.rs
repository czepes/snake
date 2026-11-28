//! Регистрация участника: ожидание ACK, конфигурации и снимка в любом порядке.

use std::net::SocketAddrV4;
use std::time::{Duration, Instant};

use crate::catalog::GameKey;
use crate::reliable::{Kind, Reliable};
use crate::wire::{
    Announcement, Body, Config, Message, NodeRole, PlayerStatus, PlayerType, StateMsg,
};

use super::{
    Ctx, Game, JOIN_DEADLINE, JOIN_DISCOVER_PERIOD, JoiningView, MasterState, accept_request, mk,
    validate_state,
};

/// Независимо собирает подтверждение регистрации, конфиг и самый новый State.
/// Отсутствие конфига не мешает подтверждать пакеты и повторять Join.
pub(super) struct Joining {
    key: GameKey,
    started: Instant,
    join_seq: i64,
    cfg: Option<Config>,
    acked: bool,
    my_id: Option<i32>,
    early_id: Option<i32>,
    master_id: Option<i32>,
    buffered: Option<(StateMsg, Option<i32>)>,
    last_discover: Instant,
    rel: Reliable,
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
    ) -> Joining {
        let mut rel = Reliable::new();
        let join = mk(
            0,
            None,
            None,
            Some(&key.name),
            Body::Join {
                ptype: PlayerType::Human,
                player_name: player,
                game_name: key.name.clone(),
                status,
            },
        );
        let sent = rel.send_request(cx.ob, cx.now, key.addr, Kind::Control, join);
        Joining {
            key,
            started: cx.now,
            join_seq: sent.seq,
            cfg,
            acked: false,
            my_id: None,
            early_id: None,
            master_id: None,
            buffered: None,
            last_discover: cx.now,
            rel,
        }
    }

    pub(super) fn view(&self) -> JoiningView<'_> {
        JoiningView {
            game: &self.key.name,
            started: self.started,
            acked: self.acked,
            has_config: self.cfg.is_some(),
        }
    }

    pub(super) fn on_announcement(
        &mut self,
        cx: &mut Ctx,
        from: SocketAddrV4,
        games: &[Announcement],
    ) -> JoinStep {
        if from == self.key.addr
            && let Some(g) = games
                .iter()
                .find(|g| g.game_name == self.key.name && g.config.is_valid())
        {
            self.cfg = Some(g.config);
            return self.try_complete(cx);
        }
        JoinStep::Wait
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
        if !self.matches_message(from, &msg) {
            return JoinStep::Wait;
        }
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
                accept_request(&mut self.rel, cx, from, &msg, my_id, self.master_id, &name);
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
        }
        None
    }

    fn on_state(&mut self, cx: &mut Ctx, from: SocketAddrV4, msg: &Message, state: &StateMsg) {
        let my_id = self.my_id.or(self.early_id);
        if accept_request(
            &mut self.rel,
            cx,
            from,
            msg,
            my_id,
            self.master_id,
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
        link.last_rx = cx.now;
        link.last_tx = cx.now;
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

    pub(super) fn tick(&mut self, cx: &mut Ctx) -> JoinStep {
        self.rel.tick(cx.ob, cx.now, &self.key.name, self.my_id);
        if self.cfg.is_none()
            && self.acked
            && cx.now.duration_since(self.last_discover) >= JOIN_DISCOVER_PERIOD
        {
            let m = mk(cx.ob.seq(), None, None, None, Body::Discover);
            cx.ob.unicast(self.key.addr, &m);
            self.last_discover = cx.now;
        }
        if cx.now.duration_since(self.started) >= JOIN_DEADLINE {
            return JoinStep::Failed(self.failure_reason());
        }
        JoinStep::Wait
    }
}
