//! Reliable UDP transport: envelope framing, msg_seq bookkeeping, ACK
//! matching, retry/resend, the bounded per-endpoint reply dedup window and
//! peer liveness tracking (Ping/timeout), as specified in `snakes_task.md`.

use std::collections::{HashMap, VecDeque};
use std::net::{SocketAddr, SocketAddrV4};
use std::time::{Duration, Instant};

use protobuf::Message as _;

use crate::model::{DEDUP_WINDOW, MAX_UNACKED_PER_PEER, PEER_TIMEOUT, PROTOCOL_VERSION};
use crate::proto::{self, GameMessage};

/// A bounded cache of replies to requests, keyed by (endpoint, msg_seq),
/// enforcing the 1024-entry sliding window from the spec.
#[derive(Default)]
pub struct ReplyCache {
    /// Highest msg_seq that advanced the window ("H").
    high_watermark: i64,
    /// msg_seq -> serialized reply bytes already sent for this request.
    replies: HashMap<i64, Vec<u8>>,
    order: VecDeque<i64>,
}

impl ReplyCache {
    pub fn new() -> Self {
        Self {
            high_watermark: -1,
            replies: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    pub fn lookup(&self, seq: i64) -> Option<&[u8]> {
        self.replies.get(&seq).map(|v| v.as_slice())
    }

    pub fn in_window(&self, seq: i64) -> bool {
        seq > self.high_watermark - DEDUP_WINDOW
    }

    /// Records a reply for `seq`, advancing the watermark. Only call for
    /// messages that are valid and authorized to extend the window.
    pub fn record(&mut self, seq: i64, reply: Vec<u8>) {
        if seq > self.high_watermark {
            self.high_watermark = seq;
        }
        self.replies.insert(seq, reply);
        self.order.push_back(seq);
        let floor = self.high_watermark - DEDUP_WINDOW;
        while let Some(&front) = self.order.front() {
            if front <= floor {
                self.order.pop_front();
                self.replies.remove(&front);
            } else {
                break;
            }
        }
    }
}

/// One outgoing message awaiting ACK.
struct Pending {
    seq: i64,
    bytes: Vec<u8>,
    last_sent: Instant,
}

/// Tracks everything the transport needs to know about one remote peer:
/// outgoing retry queue, inbound reply cache, and liveness timers.
pub struct PeerState {
    pub last_unicast_sent: Instant,
    pub last_unicast_received: Instant,
    outgoing: VecDeque<Pending>,
    pub reply_cache: ReplyCache,
}

impl PeerState {
    pub fn new(addr: SocketAddrV4) -> Self {
        let _ = addr;
        let now = Instant::now();
        Self {
            last_unicast_sent: now,
            last_unicast_received: now,
            outgoing: VecDeque::new(),
            reply_cache: ReplyCache::new(),
        }
    }

    pub fn is_timed_out(&self, now: Instant) -> bool {
        now.duration_since(self.last_unicast_received) > PEER_TIMEOUT
    }

    pub fn queue(&mut self, seq: i64, bytes: Vec<u8>) {
        if let Some(existing) = self.outgoing.iter_mut().find(|p| p.seq == seq) {
            existing.bytes = bytes;
            return;
        }
        if self.outgoing.len() >= MAX_UNACKED_PER_PEER {
            self.outgoing.pop_front();
        }
        self.outgoing.push_back(Pending {
            seq,
            bytes,
            last_sent: Instant::now() - Duration::from_secs(1),
        });
    }

    /// Replaces any queued message of the same kind (used so new Steer/State
    /// messages supersede stale ones instead of just growing the queue).
    #[allow(dead_code)]
    pub fn replace_same_kind(&mut self, seq: i64, bytes: Vec<u8>, is_same_kind: impl Fn(&[u8]) -> bool) {
        if let Some(pos) = self.outgoing.iter().position(|p| is_same_kind(&p.bytes)) {
            self.outgoing.remove(pos);
        }
        self.queue(seq, bytes);
    }

    pub fn ack(&mut self, seq: i64) {
        self.outgoing.retain(|p| p.seq != seq);
    }

    pub fn due_resends(&mut self, now: Instant, interval: Duration) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        for p in self.outgoing.iter_mut() {
            if now.duration_since(p.last_sent) >= interval {
                p.last_sent = now;
                out.push(p.bytes.clone());
            }
        }
        out
    }

    pub fn has_pending(&self) -> bool {
        !self.outgoing.is_empty()
    }
}

pub fn socket_addr_v4(addr: SocketAddr) -> Option<SocketAddrV4> {
    match addr {
        SocketAddr::V4(v4) => Some(v4),
        SocketAddr::V6(_) => None,
    }
}

pub fn make_envelope(
    msg_seq: i64,
    sender_id: Option<i32>,
    receiver_id: Option<i32>,
    game_name: Option<&str>,
) -> GameMessage {
    let mut m = GameMessage::new();
    m.set_msg_seq(msg_seq);
    m.set_protocol_version(PROTOCOL_VERSION);
    if let Some(id) = sender_id {
        m.set_sender_id(id);
    }
    if let Some(id) = receiver_id {
        m.set_receiver_id(id);
    }
    if let Some(name) = game_name {
        m.set_game_name(name.to_string());
    }
    m
}

pub fn make_ack(of: &GameMessage, sender_id: Option<i32>, receiver_id: Option<i32>) -> GameMessage {
    let mut m = GameMessage::new();
    m.set_msg_seq(of.msg_seq());
    m.set_protocol_version(PROTOCOL_VERSION);
    if let Some(id) = sender_id {
        m.set_sender_id(id);
    }
    if let Some(id) = receiver_id {
        m.set_receiver_id(id);
    }
    if of.has_game_name() {
        m.set_game_name(of.game_name().to_string());
    }
    m.set_ack(proto::game_message::AckMsg::new());
    m
}

pub fn encode(m: &GameMessage) -> Option<Vec<u8>> {
    let bytes = m.write_to_bytes().ok()?;
    if bytes.len() > crate::model::MAX_DATAGRAM_BYTES {
        return None;
    }
    Some(bytes)
}

pub fn decode(bytes: &[u8]) -> Option<GameMessage> {
    let m = GameMessage::parse_from_bytes(bytes).ok()?;
    if m.protocol_version() != PROTOCOL_VERSION {
        return None;
    }
    Some(m)
}
