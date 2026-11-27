//! Надёжная доставка поверх UDP: нумерация, подтверждения, повторы, окно
//! дедупликации входящих запросов, ping и тайм-ауты связи.
//!
//! Модуль ничего не отправляет сам: исходящие датаграммы копятся в `Outbox`,
//! а время приходит параметром. Поэтому всё ядро тестируется без сокетов.

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddrV4;
use std::time::{Duration, Instant};

use crate::wire::{self, Body, Message};

/// Через сколько повторяем неподтверждённое сообщение.
pub const RESEND_AFTER: Duration = Duration::from_millis(100);
/// Если соседу ничего не отправляли столько времени — шлём Ping.
pub const PING_AFTER: Duration = Duration::from_millis(100);
/// Сосед, от которого ничего нет столько времени, считается недоступным.
pub const LINK_TIMEOUT: Duration = Duration::from_millis(1000);
/// Ширина окна дедупликации по `msg_seq`.
pub const SEQ_WINDOW: i64 = 1024;
pub const MAX_PENDING: usize = 64;
/// Предел числа endpoint'ов с собственным окном (защита памяти).
pub const MAX_WINDOWS: usize = 64;

#[derive(Debug)]
pub enum Out {
    Unicast(SocketAddrV4, Vec<u8>),
    Multicast(Vec<u8>),
}

/// Исходящие датаграммы, счётчик `msg_seq` и журнал трассировки.
pub struct Outbox {
    pub out: Vec<Out>,
    next_seq: i64,
    /// Ошибка реализации (например, слишком большое сообщение): игра должна завершиться.
    pub fault: Option<String>,
    pub trace_on: bool,
    pub trace: Vec<String>,
}

impl Outbox {
    pub fn new(first_seq: i64) -> Self {
        Outbox {
            out: Vec::new(),
            next_seq: first_seq,
            fault: None,
            trace_on: false,
            trace: Vec::new(),
        }
    }

    /// Следующий `msg_seq`. Счётчик общий для всех адресатов и типов сообщений.
    pub fn seq(&mut self) -> i64 {
        let s = self.next_seq;
        self.next_seq += 1;
        s
    }

    pub fn unicast(&mut self, to: SocketAddrV4, m: &Message) {
        match wire::encode(m) {
            Ok(bytes) => {
                if self.trace_on {
                    self.trace.push(format!("TX {to} {}", describe(m)));
                }
                self.out.push(Out::Unicast(to, bytes));
            }
            Err(e) => {
                self.fault = Some(format!(
                    "сообщение {} не удалось закодировать: {e:?}",
                    m.body.name()
                ))
            }
        }
    }

    pub fn multicast(&mut self, m: &Message) {
        match wire::encode(m) {
            Ok(bytes) => {
                if self.trace_on {
                    self.trace.push(format!("TX multicast {}", describe(m)));
                }
                self.out.push(Out::Multicast(bytes));
            }
            Err(e) => {
                self.fault = Some(format!(
                    "сообщение {} не удалось закодировать: {e:?}",
                    m.body.name()
                ))
            }
        }
    }

    pub fn note(&mut self, text: impl FnOnce() -> String) {
        if self.trace_on {
            self.trace.push(text());
        }
    }
}

/// Короткое описание сообщения для журнала.
pub fn describe(m: &Message) -> String {
    let extra = match &m.body {
        Body::State(s) => format!(
            " order={} snakes={} players={}",
            s.order,
            s.snakes.len(),
            s.players.len()
        ),
        Body::Steer(d) => format!(" {d:?}"),
        Body::Join {
            player_name,
            game_name,
            status,
            ..
        } => format!(" player={player_name:?} game={game_name:?} {status:?}"),
        Body::Error { text, request_seq } => format!(" {text:?} request_seq={request_seq}"),
        Body::RoleChange {
            sender,
            receiver,
            order,
        } => format!(" {sender:?}->{receiver:?} order={order}"),
        Body::StatusChange(s) => format!(" {s:?}"),
        Body::Announcement(g) => format!(" games={}", g.len()),
        _ => String::new(),
    };
    format!(
        "{} seq={} sender={:?} receiver={:?} game={:?}{extra}",
        m.body.name(),
        m.seq,
        m.sender_id,
        m.receiver_id,
        m.game_name
    )
}

/// Как обращаться с очередью при появлении нового сообщения того же типа.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Ping,
    /// Новая команда курса заменяет устаревшую.
    Steer,
    /// Новое состояние заменяет устаревшее.
    State,
    /// Управляющие операции не теряются.
    Control,
}

#[derive(Clone, Debug)]
pub struct Pending {
    pub msg: Message,
    pub kind: Kind,
    pub last_sent: Instant,
}

#[derive(Debug)]
pub struct Link {
    pub pending: Vec<Pending>,
    pub last_rx: Instant,
    pub last_tx: Instant,
    pub max_req_sent: i64,
    /// ID участника по ту сторону, если он известен.
    pub peer_id: Option<i32>,
    /// Нужно ли слать Ping при простое (только для зарегистрированных участников).
    pub ping: bool,
}

impl Link {
    fn new(now: Instant) -> Self {
        Link {
            pending: Vec::new(),
            last_rx: now,
            last_tx: now,
            max_req_sent: -1,
            peer_id: None,
            ping: false,
        }
    }
}

/// Результат проверки входящего запроса по окну дедупликации.
#[derive(Debug)]
pub enum Seen {
    New,
    /// Повтор: нужно повторить сохранённый ответ (если он был) и не обрабатывать запрос заново.
    Duplicate(Option<Message>),
    /// Номер ниже окна: молча игнорируем.
    TooOld,
}

#[derive(Debug)]
pub struct Window {
    high: i64,
    replies: BTreeMap<i64, Option<Message>>,
}

impl Window {
    fn new() -> Self {
        Window {
            high: -1,
            replies: BTreeMap::new(),
        }
    }

    pub fn classify(&self, seq: i64) -> Seen {
        if self.high >= 0 && seq <= self.high - SEQ_WINDOW {
            return Seen::TooOld;
        }
        match self.replies.get(&seq) {
            Some(reply) => Seen::Duplicate(reply.clone()),
            None => Seen::New,
        }
    }

    pub fn record(&mut self, seq: i64, reply: Option<Message>) {
        self.replies.insert(seq, reply);
        if seq > self.high {
            self.high = seq;
        }
        let floor = self.high - SEQ_WINDOW;
        while let Some((&k, _)) = self.replies.iter().next() {
            if k <= floor {
                self.replies.remove(&k);
            } else {
                break;
            }
        }
    }
}

#[derive(Default)]
pub struct Reliable {
    pub links: HashMap<SocketAddrV4, Link>,
    windows: HashMap<SocketAddrV4, Window>,
}

impl Reliable {
    pub fn new() -> Self {
        Reliable::default()
    }

    pub fn link(&mut self, addr: SocketAddrV4, now: Instant) -> &mut Link {
        self.links.entry(addr).or_insert_with(|| Link::new(now))
    }

    /// Можно ли принять запрос от этого endpoint (лимит числа окон).
    pub fn can_admit(&self, addr: SocketAddrV4) -> bool {
        self.windows.contains_key(&addr) || self.windows.len() < MAX_WINDOWS
    }

    pub fn classify(&self, addr: SocketAddrV4, seq: i64) -> Seen {
        self.windows
            .get(&addr)
            .map_or(Seen::New, |w| w.classify(seq))
    }

    pub fn record(&mut self, addr: SocketAddrV4, seq: i64, reply: Option<Message>) {
        self.windows
            .entry(addr)
            .or_insert_with(Window::new)
            .record(seq, reply);
    }

    /// Ставит сообщение в очередь на подтверждение и отправляет его сразу.
    /// Возвращает отправленное сообщение (с присвоенным `msg_seq`).
    pub fn send_request(
        &mut self,
        ob: &mut Outbox,
        now: Instant,
        to: SocketAddrV4,
        kind: Kind,
        mut msg: Message,
    ) -> Message {
        let link = self.links.entry(to).or_insert_with(|| Link::new(now));
        match kind {
            Kind::Steer | Kind::State => link.pending.retain(|p| p.kind != kind),
            Kind::Ping => {
                if let Some(p) = link.pending.iter().find(|p| p.kind == Kind::Ping) {
                    return p.msg.clone();
                }
            }
            Kind::Control => {}
        }
        while link.pending.len() >= MAX_PENDING {
            match link.pending.iter().position(|p| p.kind != Kind::Control) {
                Some(i) => {
                    link.pending.remove(i);
                }
                None => break,
            }
        }
        msg.seq = ob.seq();
        ob.unicast(to, &msg);
        link.last_tx = now;
        link.max_req_sent = msg.seq;
        link.pending.push(Pending {
            msg: msg.clone(),
            kind,
            last_sent: now,
        });
        msg
    }

    /// Отправляет ACK на запрос и возвращает его (его сохраняют в окне для повторов).
    pub fn send_ack(
        &mut self,
        ob: &mut Outbox,
        now: Instant,
        to: SocketAddrV4,
        req_seq: i64,
        (sender_id, receiver_id): (Option<i32>, Option<i32>),
        game: &str,
    ) -> Message {
        let ack = Message {
            seq: req_seq,
            sender_id,
            receiver_id,
            game_name: Some(game.to_owned()),
            body: Body::Ack,
        };
        ob.unicast(to, &ack);
        if let Some(link) = self.links.get_mut(&to) {
            link.last_tx = now;
        }
        ack
    }

    /// Повторяет сохранённый ответ на дубликат запроса.
    pub fn resend_reply(
        &mut self,
        ob: &mut Outbox,
        now: Instant,
        to: SocketAddrV4,
        reply: &Message,
    ) {
        ob.unicast(to, reply);
        if let Some(link) = self.links.get_mut(&to) {
            link.last_tx = now;
        }
    }

    /// Любое корректное сообщение от соседа продлевает связь.
    pub fn touch(&mut self, from: SocketAddrV4, now: Instant) {
        if let Some(link) = self.links.get_mut(&from) {
            link.last_rx = now;
        }
    }

    /// Обрабатывает ACK: снимает подтверждённое сообщение с повторов.
    pub fn on_ack(&mut self, now: Instant, from: SocketAddrV4, seq: i64) -> Option<Pending> {
        let link = self.links.get_mut(&from)?;
        link.last_rx = now;
        let i = link.pending.iter().position(|p| p.msg.seq == seq)?;
        Some(link.pending.remove(i))
    }

    /// Повторы, Ping и поиск недоступных соседей. Возвращает адреса, от которых
    /// слишком давно ничего нет; что с ними делать, решает вызывающий.
    pub fn tick(
        &mut self,
        ob: &mut Outbox,
        now: Instant,
        game: &str,
        my_id: Option<i32>,
    ) -> Vec<SocketAddrV4> {
        let mut dead = Vec::new();
        for (&addr, link) in self.links.iter_mut() {
            let max_sent = link.max_req_sent;
            // Запрос, после которого ушло ≥ окна новых, больше не повторяем.
            link.pending.retain(|p| max_sent < p.msg.seq + SEQ_WINDOW);

            let mut sent = false;
            for p in link.pending.iter_mut() {
                if now.duration_since(p.last_sent) >= RESEND_AFTER {
                    ob.unicast(addr, &p.msg);
                    p.last_sent = now;
                    sent = true;
                }
            }
            if sent {
                link.last_tx = now;
            }

            let has_ping = link.pending.iter().any(|p| p.kind == Kind::Ping);
            if link.ping && !has_ping && now.duration_since(link.last_tx) >= PING_AFTER {
                let msg = Message {
                    seq: ob.seq(),
                    sender_id: my_id,
                    receiver_id: link.peer_id,
                    game_name: Some(game.to_owned()),
                    body: Body::Ping,
                };
                ob.unicast(addr, &msg);
                link.last_tx = now;
                link.max_req_sent = msg.seq;
                link.pending.push(Pending {
                    msg,
                    kind: Kind::Ping,
                    last_sent: now,
                });
            }

            if now.duration_since(link.last_rx) > LINK_TIMEOUT {
                dead.push(addr);
            }
        }
        dead
    }

    /// Переносит связь на новый endpoint (смена MASTER): неподтверждённые запросы
    /// сохраняют номера и содержимое, меняются адрес и `receiver_id`.
    pub fn retarget(
        &mut self,
        from: SocketAddrV4,
        to: SocketAddrV4,
        new_peer_id: Option<i32>,
        now: Instant,
    ) {
        let mut link = self.links.remove(&from).unwrap_or_else(|| Link::new(now));
        for p in &mut link.pending {
            p.msg.receiver_id = new_peer_id;
            // Повторяем сразу, не дожидаясь очередного периода.
            p.last_sent = now.checked_sub(RESEND_AFTER).unwrap_or(now);
        }
        link.peer_id = new_peer_id;
        link.last_rx = now;
        link.last_tx = now;
        link.ping = true;
        self.links.insert(to, link);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn addr(n: u8) -> SocketAddrV4 {
        SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, n), 4000)
    }

    fn ping_msg() -> Message {
        Message {
            seq: 0,
            sender_id: Some(1),
            receiver_id: Some(2),
            game_name: Some("g".into()),
            body: Body::Steer(crate::wire::Dir::Up),
        }
    }

    #[test]
    fn window_rejects_old_and_replays_duplicates() {
        let mut w = Window::new();
        assert!(matches!(w.classify(5), Seen::New));
        w.record(5, None);
        assert!(matches!(w.classify(5), Seen::Duplicate(None)));
        w.record(2000, None);
        assert!(matches!(w.classify(5), Seen::TooOld));
        assert!(matches!(w.classify(1500), Seen::New));
        assert!(matches!(w.classify(2000 - SEQ_WINDOW), Seen::TooOld));
        assert!(matches!(w.classify(2000 - SEQ_WINDOW + 1), Seen::New));
    }

    #[test]
    fn requests_are_resent_until_acked() {
        let mut ob = Outbox::new(10);
        let mut rel = Reliable::new();
        let t0 = Instant::now();
        let sent = rel.send_request(&mut ob, t0, addr(1), Kind::Control, ping_msg());
        assert_eq!(sent.seq, 10);
        assert_eq!(ob.out.len(), 1);

        rel.tick(&mut ob, t0 + Duration::from_millis(50), "g", Some(1));
        assert_eq!(ob.out.len(), 1, "рано повторять");
        rel.tick(&mut ob, t0 + Duration::from_millis(120), "g", Some(1));
        assert_eq!(ob.out.len(), 2);

        assert!(
            rel.on_ack(t0 + Duration::from_millis(130), addr(1), 10)
                .is_some()
        );
        rel.tick(&mut ob, t0 + Duration::from_millis(400), "g", Some(1));
        assert_eq!(ob.out.len(), 2, "после ACK повторов нет");
    }

    #[test]
    fn newer_state_replaces_older_but_control_stays() {
        let mut ob = Outbox::new(0);
        let mut rel = Reliable::new();
        let t0 = Instant::now();
        rel.send_request(&mut ob, t0, addr(1), Kind::State, ping_msg());
        rel.send_request(&mut ob, t0, addr(1), Kind::Control, ping_msg());
        rel.send_request(&mut ob, t0, addr(1), Kind::State, ping_msg());
        let kinds: Vec<_> = rel.links[&addr(1)].pending.iter().map(|p| p.kind).collect();
        assert_eq!(kinds, vec![Kind::Control, Kind::State]);
    }

    #[test]
    fn idle_registered_link_gets_pinged_and_silent_link_times_out() {
        let mut ob = Outbox::new(0);
        let mut rel = Reliable::new();
        let t0 = Instant::now();
        rel.link(addr(1), t0).ping = true;
        assert!(
            rel.tick(&mut ob, t0 + Duration::from_millis(50), "g", Some(1))
                .is_empty()
        );
        assert!(ob.out.is_empty());
        rel.tick(&mut ob, t0 + Duration::from_millis(110), "g", Some(1));
        assert_eq!(ob.out.len(), 1, "должен уйти Ping");
        let dead = rel.tick(&mut ob, t0 + Duration::from_millis(1100), "g", Some(1));
        assert_eq!(dead, vec![addr(1)]);
    }

    #[test]
    fn retarget_keeps_sequence_numbers() {
        let mut ob = Outbox::new(100);
        let mut rel = Reliable::new();
        let t0 = Instant::now();
        rel.send_request(&mut ob, t0, addr(1), Kind::Control, ping_msg());
        rel.retarget(addr(1), addr(2), Some(7), t0 + Duration::from_millis(5));
        assert!(!rel.links.contains_key(&addr(1)));
        let p = &rel.links[&addr(2)].pending[0];
        assert_eq!(p.msg.seq, 100);
        assert_eq!(p.msg.receiver_id, Some(7));
    }
}
